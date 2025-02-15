use std::path::Path;

use fm::FileManager;
use nargo::artifacts::program::ProgramArtifact;
use nargo::ops::{collect_errors, compile_contract, compile_program, report_errors};
use nargo::package::Package;
use nargo::workspace::Workspace;
use nargo::{insert_all_files_for_workspace_into_file_manager, parse_all};
use nargo_toml::{get_package_manifest, resolve_workspace_from_toml, PackageSelection};
use noirc_driver::file_manager_with_stdlib;
use noirc_driver::NOIR_ARTIFACT_VERSION_STRING;
use noirc_driver::{CompilationResult, CompileOptions, CompiledContract, CompiledProgram};

use noirc_frontend::graph::CrateName;

use clap::Args;
use noirc_frontend::hir::ParsedFiles;

use crate::backends::Backend;
use crate::errors::CliError;

use super::fs::program::only_acir;
use super::fs::program::{read_program_from_file, save_contract_to_file, save_program_to_file};
use super::NargoConfig;
use rayon::prelude::*;

/// Compile the program and its secret execution trace into ACIR format
#[derive(Debug, Clone, Args)]
pub(crate) struct CompileCommand {
    /// The name of the package to compile
    #[clap(long, conflicts_with = "workspace")]
    package: Option<CrateName>,

    /// Compile all packages in the workspace
    #[clap(long, conflicts_with = "package")]
    workspace: bool,

    #[clap(flatten)]
    compile_options: CompileOptions,
}

pub(crate) fn run(
    backend: &Backend,
    args: CompileCommand,
    config: NargoConfig,
) -> Result<(), CliError> {
    let toml_path = get_package_manifest(&config.program_dir)?;
    let default_selection =
        if args.workspace { PackageSelection::All } else { PackageSelection::DefaultOrAll };
    let selection = args.package.map_or(default_selection, PackageSelection::Selected);

    let workspace = resolve_workspace_from_toml(
        &toml_path,
        selection,
        Some(NOIR_ARTIFACT_VERSION_STRING.to_owned()),
    )?;
    let circuit_dir = workspace.target_directory_path();

    let mut workspace_file_manager = file_manager_with_stdlib(&workspace.root_dir);
    insert_all_files_for_workspace_into_file_manager(&workspace, &mut workspace_file_manager);
    let parsed_files = parse_all(&workspace_file_manager);

    let expression_width = args
        .compile_options
        .expression_width
        .unwrap_or_else(|| backend.get_backend_info_or_default());
    let compiled_workspace = compile_workspace(
        &workspace_file_manager,
        &parsed_files,
        &workspace,
        &args.compile_options,
    );

    let (compiled_programs, compiled_contracts) = report_errors(
        compiled_workspace,
        &workspace_file_manager,
        args.compile_options.deny_warnings,
        args.compile_options.silence_warnings,
    )?;

    let (binary_packages, contract_packages): (Vec<_>, Vec<_>) = workspace
        .into_iter()
        .filter(|package| !package.is_library())
        .cloned()
        .partition(|package| package.is_binary());

    // Save build artifacts to disk.
    let only_acir = args.compile_options.only_acir;
    for (package, program) in binary_packages.into_iter().zip(compiled_programs) {
        let program = nargo::ops::transform_program(program, expression_width);
        save_program(program.clone(), &package, &workspace.target_directory_path(), only_acir);
    }
    for (package, contract) in contract_packages.into_iter().zip(compiled_contracts) {
        let contract = nargo::ops::transform_contract(contract, expression_width);
        save_contract(contract, &package, &circuit_dir);
    }

    Ok(())
}

pub(super) fn compile_workspace(
    file_manager: &FileManager,
    parsed_files: &ParsedFiles,
    workspace: &Workspace,
    compile_options: &CompileOptions,
) -> CompilationResult<(Vec<CompiledProgram>, Vec<CompiledContract>)> {
    let (binary_packages, contract_packages): (Vec<_>, Vec<_>) = workspace
        .into_iter()
        .filter(|package| !package.is_library())
        .cloned()
        .partition(|package| package.is_binary());

    // Compile all of the packages in parallel.
    let program_results: Vec<CompilationResult<CompiledProgram>> = binary_packages
        .par_iter()
        .map(|package| {
            let program_artifact_path = workspace.package_build_path(package);
            let cached_program: Option<CompiledProgram> =
                read_program_from_file(program_artifact_path)
                    .ok()
                    .filter(|p| p.noir_version == NOIR_ARTIFACT_VERSION_STRING)
                    .map(|p| p.into());

            compile_program(file_manager, parsed_files, package, compile_options, cached_program)
        })
        .collect();
    let contract_results: Vec<CompilationResult<CompiledContract>> = contract_packages
        .par_iter()
        .map(|package| compile_contract(file_manager, parsed_files, package, compile_options))
        .collect();

    // Collate any warnings/errors which were encountered during compilation.
    let compiled_programs = collect_errors(program_results);
    let compiled_contracts = collect_errors(contract_results);

    match (compiled_programs, compiled_contracts) {
        (Ok((programs, program_warnings)), Ok((contracts, contract_warnings))) => {
            let warnings = [program_warnings, contract_warnings].concat();
            Ok(((programs, contracts), warnings))
        }
        (Err(program_errors), Err(contract_errors)) => {
            Err([program_errors, contract_errors].concat())
        }
        (Err(errors), _) | (_, Err(errors)) => Err(errors),
    }
}

pub(super) fn save_program(
    program: CompiledProgram,
    package: &Package,
    circuit_dir: &Path,
    only_acir_opt: bool,
) {
    let program_artifact = ProgramArtifact::from(program.clone());
    if only_acir_opt {
        only_acir(&program_artifact, circuit_dir);
    } else {
        save_program_to_file(&program_artifact, &package.name, circuit_dir);
    }
}

fn save_contract(contract: CompiledContract, package: &Package, circuit_dir: &Path) {
    let contract_name = contract.name.clone();
    save_contract_to_file(
        &contract.into(),
        &format!("{}-{}", package.name, contract_name),
        circuit_dir,
    );
}
