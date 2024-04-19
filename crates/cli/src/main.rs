mod logger;

use anyhow::{anyhow, bail, Error};
use base::commands::start_server;
use base::deno_runtime::MAYBE_DENO_VERSION;
use base::rt_worker::worker_pool::{SupervisorPolicy, WorkerPoolPolicy};
use base::server::{ServerFlags, Tls, WorkerEntrypoints};
use base::{DecoratorType, InspectorOption};
use clap::builder::{BoolishValueParser, FalseyValueParser, TypedValueParser};
use clap::{arg, crate_version, value_parser, ArgAction, ArgGroup, ArgMatches, Command};
use deno_core::url::Url;
use log::warn;
use sb_graph::emitter::EmitterFactory;
use sb_graph::import_map::load_import_map;
use sb_graph::{
    extract_from_file, generate_binary_eszip, include_glob_patterns_in_eszip, STATIC_FS_PREFIX,
};
use std::fs::File;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

fn cli() -> Command {
    Command::new("edge-runtime")
        .about("A server based on Deno runtime, capable of running JavaScript, TypeScript, and WASM services")
        .version(format!(
            "{}\ndeno {} ({}, {})",
            crate_version!(),
            env!("DENO_VERSION"),
            env!("PROFILE"),
            env!("TARGET")
        ))
        .arg_required_else_help(true)
        .arg(
            arg!(-v --verbose "Use verbose output")
                .conflicts_with("quiet")
                .global(true)
                .action(ArgAction::SetTrue),
        )
        .arg(
            arg!(-q --quiet "Do not print any log messages")
                .conflicts_with("verbose")
                .global(true)
                .action(ArgAction::SetTrue),
        )
        .arg(
            arg!(--"log-source" "Include source file and line in log messages")
                .global(true)
                .action(ArgAction::SetTrue),
        )
        .subcommand(
            Command::new("start")
                .about("Start the server")
                .arg(arg!(-i --ip <HOST> "Host IP address to listen on").default_value("0.0.0.0"))
                .arg(
                    arg!(-p --port <PORT> "Port to listen on")
                        .env("EDGE_RUNTIME_PORT")
                        .default_value("9000")
                        .value_parser(value_parser!(u16))
                )
                .arg(
                    arg!(--tls [PORT])
                        .env("EDGE_RUNTIME_TLS")
                        .num_args(0..=1)
                        .default_missing_value("443")
                        .value_parser(value_parser!(u16))
                        .requires("key")
                        .requires("cert")
                )
                .arg(
                    arg!(--key <Path> "Path to PEM-encoded key to be used to TLS")
                        .env("EDGE_RUNTIME_TLS_KEY_PATH")
                        .value_parser(value_parser!(PathBuf))
                )
                .arg(
                    arg!(--cert <Path> "Path to PEM-encoded X.509 certificate to be used to TLS")
                        .env("EDGE_RUNTIME_TLS_CERT_PATH")
                        .value_parser(value_parser!(PathBuf))
                )
                .arg(arg!(--"main-service" <DIR> "Path to main service directory or eszip").default_value("examples/main"))
                .arg(arg!(--"disable-module-cache" "Disable using module cache").default_value("false").value_parser(FalseyValueParser::new()))
                .arg(arg!(--"import-map" <Path> "Path to import map file"))
                .arg(arg!(--"event-worker" <Path> "Path to event worker directory"))
                .arg(arg!(--"main-entrypoint" <Path> "Path to entrypoint in main service (only for eszips)"))
                .arg(arg!(--"events-entrypoint" <Path> "Path to entrypoint in events worker (only for eszips)"))
                .arg(
                    arg!(--"policy" <POLICY> "Policy to enforce in the worker pool")
                        .default_value("per_worker")
                        .value_parser(["per_worker", "per_request", "oneshot"])
                )
                .arg(
                    arg!(--"decorator" <TYPE> "Type of decorator to use on the main worker and event worker. If not specified, the decorator feature is disabled.")
                        .value_parser(["tc39", "typescript", "typescript_with_metadata"])
                )
                .arg(
                    arg!(--"graceful-exit-timeout" <SECONDS> "Maximum time in seconds that can wait for workers before terminating forcibly")
                        .default_value("0")
                        .value_parser(
                            value_parser!(u64)
                                .range(0..u64::MAX)
                        )
                )
                .arg(
                    arg!(--"max-parallelism" <COUNT> "Maximum count of workers that can exist in the worker pool simultaneously")
                        .value_parser(
                            // NOTE: Acceptable bounds were chosen arbitrarily.
                            value_parser!(u32)
                                .range(1..9999)
                                .map(|it| -> usize { it as usize })
                        )
                )
                .arg(
                    arg!(--"request-wait-timeout" <MILLISECONDS> "Maximum time in milliseconds that can wait to establish a connection with a worker")
                        .value_parser(value_parser!(u64))
                )
                .arg(
                    arg!(--"inspect" [HOST_AND_PORT] "Activate inspector on host:port (default: 127.0.0.1:9229)")
                        .num_args(0..=1)
                        .value_parser(value_parser!(SocketAddr))
                        .require_equals(true)
                        .default_missing_value("127.0.0.1:9229")
                )
                .arg(
                    arg!(--"inspect-brk" [HOST_AND_PORT] "Activate inspector on host:port, wait for debugger to connect and break at the start of user script")
                        .num_args(0..=1)
                        .value_parser(value_parser!(SocketAddr))
                        .require_equals(true)
                        .default_missing_value("127.0.0.1:9229")
                )
                .arg(
                    arg!(--"inspect-wait" [HOST_AND_PORT] "Activate inspector on host:port and wait for debugger to connect before running user code")
                        .num_args(0..=1)
                        .value_parser(value_parser!(SocketAddr))
                        .require_equals(true)
                        .default_missing_value("127.0.0.1:9229")
                )
                .group(
                    ArgGroup::new("inspector")
                        .args(["inspect", "inspect-brk", "inspect-wait"])
                )
                .arg(
                    arg!(--"inspect-main" "Allow creating inspector for main worker")
                        .requires("inspector")
                        .action(ArgAction::SetTrue)
                )
                .arg(arg!(--"static" <Path> "Glob pattern for static files to be included"))
                .arg(arg!(--"tcp-nodelay" [BOOL] "Disables Nagle's algorithm")
                    .num_args(0..=1)
                    .value_parser(BoolishValueParser::new())
                    .require_equals(true)
                    .default_value("true")
                    .default_missing_value("true")
                )
        )
        .subcommand(
            Command::new("bundle")
                .about("Creates an 'eszip' file that can be executed by the EdgeRuntime. Such file contains all the modules in contained in a single binary.")
                .arg(arg!(--"output" <DIR> "Path to output eszip file").default_value("bin.eszip"))
                .arg(arg!(--"entrypoint" <Path> "Path to entrypoint to bundle as an eszip").required(true))
                .arg(arg!(--"static" <Path> "Glob pattern for static files to be included"))
                .arg(arg!(--"import-map" <Path> "Path to import map file"))
                .arg(
                    arg!(--"decorator" <TYPE> "Type of decorator to use when bundling. If not specified, the decorator feature is disabled.")
                        .value_parser(["tc39", "typescript", "typescript_with_metadata"])
                )
        ).subcommand(
        Command::new("unbundle")
            .about("Unbundles an .eszip file into the specified directory")
            .arg(arg!(--"output" <DIR> "Path to extract the ESZIP content").default_value("./"))
            .arg(arg!(--"eszip" <DIR> "Path of eszip to extract").required(true))
    )
}

fn main() -> Result<(), anyhow::Error> {
    MAYBE_DENO_VERSION.get_or_init(|| env!("DENO_VERSION").to_string());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("sb-main")
        .build()
        .unwrap();

    // TODO: Tokio runtime shouldn't be needed here (Address later)
    let local = tokio::task::LocalSet::new();
    let res: Result<(), Error> = local.block_on(&runtime, async {
        let matches = cli().get_matches();

        if !matches.get_flag("quiet") {
            let verbose = matches.get_flag("verbose");
            let include_source = matches.get_flag("log-source");
            logger::init(verbose, include_source);
        }

        #[allow(clippy::single_match)]
        #[allow(clippy::arc_with_non_send_sync)]
        match matches.subcommand() {
            Some(("start", sub_matches)) => {
                let ip = sub_matches.get_one::<String>("ip").cloned().unwrap();
                let port = sub_matches.get_one::<u16>("port").copied().unwrap();

                let maybe_tls = if let Some(port) = sub_matches.get_one::<u16>("tls").copied() {
                    let Some((key_slice, cert_slice)) = sub_matches.get_one::<PathBuf>("key").and_then(|it| std::fs::read(it).ok())
                    .zip(
                        sub_matches.get_one::<PathBuf>("cert").and_then(|it| std::fs::read(it).ok())
                    ) else {
                        bail!("unable to load the key file or cert file");
                    };

                    Some(Tls::new(port, &key_slice, &cert_slice)?)
                } else {
                    None
                };

                let main_service_path = sub_matches
                    .get_one::<String>("main-service")
                    .cloned()
                    .unwrap();
                let import_map_path = sub_matches.get_one::<String>("import-map").cloned();

                let no_module_cache = sub_matches
                    .get_one::<bool>("disable-module-cache")
                    .cloned()
                    .unwrap();

                let allow_main_inspector = sub_matches
                    .get_one::<bool>("inspect-main")
                    .cloned()
                    .unwrap();

                let event_service_manager_path =
                    sub_matches.get_one::<String>("event-worker").cloned();
                let maybe_main_entrypoint =
                    sub_matches.get_one::<String>("main-entrypoint").cloned();
                let maybe_events_entrypoint =
                    sub_matches.get_one::<String>("events-entrypoint").cloned();

                let maybe_supervisor_policy = sub_matches
                    .get_one::<String>("policy")
                    .map(|it| it.parse::<SupervisorPolicy>().unwrap());

                let graceful_exit_timeout = sub_matches.get_one::<u64>("graceful-exit-timeout").cloned();
                let maybe_max_parallelism =
                    sub_matches.get_one::<usize>("max-parallelism").cloned();
                let maybe_request_wait_timeout =
                    sub_matches.get_one::<u64>("request-wait-timeout").cloned();
                let static_patterns = if let Some(val_ref) = sub_matches
                    .get_many::<String>("static") {
                    val_ref.map(|s| s.as_str()).collect::<Vec<&str>>()
                } else {
                    vec![]
                };

                let static_patterns: Vec<String> =
                    static_patterns.into_iter().map(|s| s.to_string()).collect();

                let inspector = sub_matches.get_one::<clap::Id>("inspector").zip(
                    sub_matches
                        .get_one("inspect")
                        .or(sub_matches.get_one("inspect-brk"))
                        .or(sub_matches.get_one::<SocketAddr>("inspect-wait")),
                );

                let maybe_inspector_option = if inspector.is_some()
                    && !maybe_supervisor_policy
                        .as_ref()
                        .map(SupervisorPolicy::is_oneshot)
                        .unwrap_or(false)
                {
                    bail!(
                        "specifying `oneshot` policy is required to enable the inspector feature"
                    );
                } else if let Some((key, addr)) = inspector {
                    Some(get_inspector_option(key.as_str(), addr).unwrap())
                } else {
                    None
                };

                let tcp_nodelay =sub_matches.get_one::<bool>("tcp-nodelay")
                .copied()
                .unwrap();

                start_server(
                    ip.as_str(),
                    port,
                    maybe_tls,
                    main_service_path,
                    event_service_manager_path,
                    get_decorator_option(sub_matches),
                    Some(WorkerPoolPolicy::new(
                        maybe_supervisor_policy,
                        if let Some(true) = maybe_supervisor_policy
                            .as_ref()
                            .map(SupervisorPolicy::is_oneshot)
                        {
                            if let Some(parallelism) = maybe_max_parallelism {
                                if parallelism == 0 || parallelism > 1 {
                                    warn!("if `oneshot` policy is enabled, the maximum parallelism is fixed to `1` as forcibly");
                                }
                            }

                            Some(1)
                        } else {
                            maybe_max_parallelism
                        },
                        maybe_request_wait_timeout,
                    )),
                    import_map_path,
                    ServerFlags {
                        no_module_cache,
                        allow_main_inspector,
                        tcp_nodelay,
                        graceful_exit_deadline_sec: graceful_exit_timeout.unwrap_or(0),
                    },
                    None,
                    WorkerEntrypoints {
                        main: maybe_main_entrypoint,
                        events: maybe_events_entrypoint,
                    },
                    None,
                    static_patterns,
                    maybe_inspector_option
                )
                .await?;
            }
            Some(("bundle", sub_matches)) => {
                let output_path = sub_matches.get_one::<String>("output").cloned().unwrap();
                let import_map_path = sub_matches.get_one::<String>("import-map").cloned();
                let maybe_decorator = get_decorator_option(sub_matches);
                let static_patterns = if let Some(val_ref) = sub_matches
                    .get_many::<String>("static") {
                    val_ref.map(|s| s.as_str()).collect::<Vec<&str>>()
                } else {
                    vec![]
                };

                let entry_point_path = sub_matches
                    .get_one::<String>("entrypoint")
                    .cloned()
                    .unwrap();

                let path = PathBuf::from(entry_point_path.as_str());
                if !path.exists() {
                    bail!("entrypoint path does not exist ({})", path.display());
                }

                let mut emitter_factory = EmitterFactory::new();
                let maybe_import_map = load_import_map(import_map_path.clone())
                    .map_err(|e| anyhow!("import map path is invalid ({})", e))?;
                let mut maybe_import_map_url = None;
                if maybe_import_map.is_some() {
                    let abs_import_map_path =
                        std::env::current_dir().map(|p| p.join(import_map_path.unwrap()))?;
                    maybe_import_map_url = Some(
                        Url::from_file_path(abs_import_map_path)
                            .map_err(|_| anyhow!("failed get import map url"))?
                            .to_string(),
                    );
                }

                emitter_factory.set_decorator_type(maybe_decorator);
                emitter_factory.set_import_map(maybe_import_map.clone());

                let mut eszip = generate_binary_eszip(
                    path.canonicalize().unwrap(),
                    Arc::new(emitter_factory),
                    None,
                    maybe_import_map_url,
                )
                .await?;

                include_glob_patterns_in_eszip(static_patterns, &mut eszip, Some(STATIC_FS_PREFIX.to_string())).await;

                let bin = eszip.into_bytes();

                if output_path == "-" {
                    let stdout = std::io::stdout();
                    let mut handle = stdout.lock();

                    handle.write_all(&bin)?
                } else {
                    let mut file = File::create(output_path.as_str())?;
                    file.write_all(&bin)?
                }
            }
            Some(("unbundle", sub_matches)) => {
                let output_path = sub_matches.get_one::<String>("output").cloned().unwrap();
                let eszip_path = sub_matches.get_one::<String>("eszip").cloned().unwrap();

                let output_path = PathBuf::from(output_path.as_str());
                let eszip_path = PathBuf::from(eszip_path.as_str());

                extract_from_file(eszip_path, output_path.clone()).await;

                println!(
                    "Eszip extracted successfully inside path {}",
                    output_path.to_str().unwrap()
                );
            }
            _ => {
                // unrecognized command
            }
        }
        Ok(())
    });

    res
}

fn get_decorator_option(sub_matches: &ArgMatches) -> Option<DecoratorType> {
    sub_matches
        .get_one::<String>("decorator")
        .cloned()
        .and_then(|it| match it.to_lowercase().as_str() {
            "tc39" => Some(DecoratorType::Tc39),
            "typescript" => Some(DecoratorType::Typescript),
            "typescript_with_metadata" => Some(DecoratorType::TypescriptWithMetadata),
            _ => None,
        })
}

fn get_inspector_option(key: &str, addr: &SocketAddr) -> Result<InspectorOption, anyhow::Error> {
    match key {
        "inspect" => Ok(InspectorOption::Inspect(*addr)),
        "inspect-brk" => Ok(InspectorOption::WithBreak(*addr)),
        "inspect-wait" => Ok(InspectorOption::WithWait(*addr)),
        key => bail!("invalid inspector key: {}", key),
    }
}
