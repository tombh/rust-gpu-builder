use std::{
    collections::BTreeMap,
    error::Error,
    path::{Path, PathBuf},
    str::FromStr,
};

use rust_gpu_builder_shared::{RustGpuBuilderModules, RustGpuBuilderOutput};

use clap::{error::ErrorKind, Parser};

use async_channel::{unbounded, Receiver, Sender};
use async_executor::Executor;
use easy_parallel::Parallel;
use futures_lite::future;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use spirv_builder::{
    Capability, CompileResult, MetadataPrintout, SpirvBuilder, SpirvBuilderError, SpirvMetadata,
};

use tracing::{error, info};

/// Clap application struct.
#[derive(Debug, Clone, Parser)]
#[command(author, version, about, long_about = None)]
struct ShaderBuilder {
    /// Shader crate to compile.
    path_to_crate: PathBuf,
    /// If set, combined SPIR-V and entrypoint metadata will be written to this file on succesful compile.
    output_path: Option<PathBuf>,
    /// rust-gpu compile target.
    #[arg(short, long, default_value = "spirv-unknown-vulkan1.2")]
    target: String,
    /// Treat warnings as errors during compilation.
    #[arg(long, default_value = "false")]
    deny_warnings: bool,
    /// Compile shaders in release mode.
    #[arg(long, default_value = "true")]
    release: bool,
    /// Enables the provided SPIR-V capability.
    #[arg(long, value_parser=Self::spirv_capability)]
    capability: Vec<Capability>,
    /// Compile one .spv file per entry point.
    #[arg(long, default_value = "false")]
    multimodule: bool,
    /// Set the level of metadata included in the SPIR-V binary.
    #[arg(long, value_parser=Self::spirv_metadata, default_value = "none")]
    spirv_metadata: SpirvMetadata,
    /// Allow store from one struct type to a different type with compatible layout and members.
    #[arg(long, default_value = "false")]
    relax_struct_store: bool,
    /// Allow allocating an object of a pointer type and returning a pointer value from a function
    /// in logical addressing mode.
    #[arg(long, default_value = "false")]
    relax_logical_pointer: bool,
    /// Enable VK_KHR_relaxed_block_layout when checking standard uniform,
    /// storage buffer, and push constant layouts.
    /// This is the default when targeting Vulkan 1.1 or later.
    #[arg(long, default_value = "false")]
    relax_block_layout: bool,
    /// Enable VK_KHR_uniform_buffer_standard_layout when checking standard uniform buffer layouts.
    #[arg(long, default_value = "false")]
    uniform_buffer_standard_layout: bool,
    /// Enable VK_EXT_scalar_block_layout when checking standard uniform, storage buffer, and push
    /// constant layouts.
    /// Scalar layout rules are more permissive than relaxed block layout so in effect this will
    /// override the --relax-block-layout option.
    #[arg(long, default_value = "false")]
    scalar_block_layout: bool,
    /// Skip checking standard uniform / storage buffer layout. Overrides any --relax-block-layout
    /// or --scalar-block-layout option.
    #[arg(long, default_value = "false")]
    skip_block_layout: bool,
    /// Preserve unused descriptor bindings. Useful for reflection.
    #[arg(long, default_value = "false")]
    preserve_bindings: bool,
    /// If set, will watch the provided directory and recompile on change.
    ///
    /// Can be specified multiple times to watch more than one directory.
    #[arg(short, long)]
    watch_paths: Option<Vec<String>>,
}

impl ShaderBuilder {
    /// Clap value parser for `SpirvMetadata`.
    fn spirv_metadata(s: &str) -> Result<SpirvMetadata, clap::Error> {
        match s {
            "none" => Ok(SpirvMetadata::None),
            "name-variables" => Ok(SpirvMetadata::NameVariables),
            "full" => Ok(SpirvMetadata::Full),
            _ => Err(clap::Error::new(ErrorKind::InvalidValue)),
        }
    }

    /// Clap value parser for `Capability`.
    fn spirv_capability(s: &str) -> Result<Capability, clap::Error> {
        match Capability::from_str(s) {
            Ok(capability) => Ok(capability),
            Err(_) => Err(clap::Error::new(ErrorKind::InvalidValue)),
        }
    }

    /// Builds a shader with the provided set of options.
    pub fn build_shader(&self) -> Result<CompileResult, SpirvBuilderError> {
        let mut builder = SpirvBuilder::new(&self.path_to_crate, &self.target)
            .deny_warnings(self.deny_warnings)
            .release(self.release)
            .multimodule(self.multimodule)
            .spirv_metadata(self.spirv_metadata)
            .relax_struct_store(self.relax_struct_store)
            .relax_logical_pointer(self.relax_logical_pointer)
            .relax_block_layout(self.relax_block_layout)
            .uniform_buffer_standard_layout(self.uniform_buffer_standard_layout)
            .scalar_block_layout(self.scalar_block_layout)
            .skip_block_layout(self.skip_block_layout)
            .preserve_bindings(self.preserve_bindings)
            .print_metadata(MetadataPrintout::None);

        for capability in &self.capability {
            builder = builder.capability(*capability);
        }

        builder.build()
    }
}

enum Msg {
    Change,
    Build(Result<CompileResult, SpirvBuilderError>),
}

/// Instantiate an async watcher and return it alongside a channel to receive events on.
fn async_watcher() -> notify::Result<(RecommendedWatcher, Receiver<notify::Result<Event>>)> {
    let (tx, rx) = unbounded();

    // Automatically select the best implementation for your platform.
    // You can also access each implementation directly e.g. INotifyWatcher.
    let watcher = RecommendedWatcher::new(
        move |res| {
            future::block_on(async {
                tx.send(res).await.unwrap();
            })
        },
        Default::default(),
    )?;

    Ok((watcher, rx))
}

/// Watch a file or directory, sending relevant events through the provided channel.
async fn async_watch<P: AsRef<Path>>(
    path: P,
    change_tx: Sender<Msg>,
) -> Result<(), Box<dyn Error>> {
    let path = path.as_ref();
    let path = std::fs::canonicalize(path)
        .unwrap_or_else(|e| panic!("Failed to canonicalize path {path:?}: {e:}"));

    let (mut watcher, rx) = async_watcher()?;

    // Add a path to be watched. All files and directories at that path and
    // below will be monitored for changes.
    let watch_path = if path.is_dir() {
        path.clone()
    } else {
        path.parent().unwrap().to_owned()
    };
    watcher.watch(watch_path.as_ref(), RecursiveMode::Recursive)?;

    while let Ok(res) = rx.recv().await {
        match res {
            Ok(event) => {
                if path.is_dir()
                    || event
                        .paths
                        .iter()
                        .find(|candidate| **candidate == path)
                        .is_some()
                {
                    change_tx.send(Msg::Change).await.unwrap();
                }
            }
            Err(e) => error!("Watch error: {:?}", e),
        }
    }

    Ok(())
}

async fn handle_compile_result(result: CompileResult, output_path: Option<PathBuf>) {
    info!("Entry Points:");
    for entry in &result.entry_points {
        println!("{entry:}");
    }

    let entry_points = result.entry_points;

    println!();

    info!("Modules:");
    match &result.module {
        spirv_builder::ModuleResult::SingleModule(single) => {
            println!("{single:?}");
        }

        spirv_builder::ModuleResult::MultiModule(multi) => {
            for (k, module) in multi {
                println!("{k:}: {module:?}");
            }
        }
    };

    let Some(output_path) = output_path else {
                                    return
                                };

    let modules = match result.module {
        spirv_builder::ModuleResult::SingleModule(single) => {
            let module = async_fs::read(single)
                .await
                .expect("Failed to read module file");
            RustGpuBuilderModules::Single(module)
        }

        spirv_builder::ModuleResult::MultiModule(multi) => {
            let mut out = BTreeMap::default();
            for (k, module) in multi {
                let module = async_fs::read(module)
                    .await
                    .expect("Failed to read module file");
                out.insert(k, module);
            }
            RustGpuBuilderModules::Multi(out)
        }
    };

    let out = RustGpuBuilderOutput {
        entry_points,
        modules,
    };

    let out = serde_json::to_string_pretty(&out).expect("Failed to serialize output");
    async_fs::write(&output_path, out)
        .await
        .expect("Failed to write output");
    println!();
    info!("Wrote output to {output_path:?}");
}

fn main() {
    tracing_subscriber::fmt().init();

    let mut args = ShaderBuilder::parse();

    println!();
    info!("Shader Builder");
    println!();

    info!("Building shader...");
    println!();
    if let Ok(result) = args.build_shader() {
        future::block_on(handle_compile_result(result, args.output_path.clone()));
    } else {
        error!("Build failed!");
    }
    println!();

    let Some(watch_paths) = args.watch_paths.take() else {
        return
    };

    let ex = Executor::new();
    let (change_tx, change_rx) = unbounded::<Msg>();
    let (build_tx, build_rx) = unbounded::<Msg>();

    Parallel::new()
        .each(watch_paths, |path| {
            info!("Watching {path:} for changes...");
            ex.spawn(async {
                async_watch(path, change_tx).await.unwrap();
            })
            .detach();
        })
        .add(|| {
            let mut building = false;
            loop {
                match future::block_on(futures_lite::future::race(
                    change_rx.recv(),
                    build_rx.recv(),
                )) {
                    Ok(Msg::Change) => {
                        if !building {
                            building = true;
                            println!();
                            info!("Building shader...");
                            println!();
                            ex.spawn({
                                let build_tx = build_tx.clone();
                                let args = args.clone();
                                async move {
                                    build_tx
                                        .send(Msg::Build(args.build_shader()))
                                        .await
                                        .unwrap();
                                }
                            })
                            .detach();
                        }
                    }
                    Ok(Msg::Build(result)) => {
                        if let Ok(result) = result {
                            let output_path = args.output_path.clone();
                            ex.spawn(handle_compile_result(result, output_path))
                                .detach();
                        } else {
                            error!("Build failed!");
                        }
                        println!();
                        building = false;
                    }
                    Err(e) => {
                        panic!("{e:}")
                    }
                }
            }
        })
        .finish(|| loop {
            future::block_on(ex.tick())
        });
}
