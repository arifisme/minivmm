mod admission;
mod agent;
mod api;
mod builder;
mod cgroup;
mod config;
mod jail;
mod logs;
mod metrics;
mod platform;
mod pool;
mod store;
mod vm;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Parser;
use mv_proto::agent::Runtime;
use tokio::signal::unix::{SignalKind, signal};

use admission::{Admission, Limits, meminfo_mib};
use builder::{Builder, Images};
use cgroup::CgroupRoot;
use config::{LogFormat, Options};
use jail::JailConfig;
use metrics::Metrics;
use platform::{Platform, Settings};
use store::Store;
use vm::Launcher;

fn init_tracing(format: LogFormat) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .init(),
    }
}

/// Each VM holds a few descriptors in the daemon (agent socket, console pipes), so the
/// default soft limit of 1024 would cap the number of VMs.
fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit receive a valid rlimit structure.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 && limit.rlim_cur < limit.rlim_max
        {
            limit.rlim_cur = limit.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

fn default_jailer() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name("mv-jailer"))
}

fn check_inputs(options: &Options, jailer: &Path) -> anyhow::Result<()> {
    let mut required = vec![options.kernel.clone(), options.vmm.clone()];
    if !options.no_jail {
        required.push(jailer.to_path_buf());
    }
    for path in required {
        if !path.is_file() {
            bail!("{} does not exist", path.display());
        }
    }
    let images =
        Runtime::ALL.map(|runtime| options.runtime_dir.join(format!("{}.ext4", runtime.name())));
    if !images.iter().any(|image| image.is_file()) {
        bail!(
            "no runtime images in {} (run scripts/build-runtimes.sh)",
            options.runtime_dir.display()
        );
    }
    Ok(())
}

fn admission_limits(options: &Options) -> Limits {
    let total = meminfo_mib("MemTotal").unwrap_or(0);
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    let base = options
        .memory_limit_mib
        .unwrap_or_else(|| total.saturating_sub(options.memory_reserve_mib));
    Limits {
        memory_total_mib: total,
        memory_limit_mib: (base as f64 * options.memory_overcommit) as u64,
        memory_reserve_mib: options.memory_reserve_mib,
        cpus,
        vcpu_limit: options.vcpu_limit.unwrap_or(cpus * 2),
    }
}

async fn run(options: Options) -> anyhow::Result<()> {
    let jailer = match &options.jailer {
        Some(path) => path.clone(),
        None => default_jailer()?,
    };
    check_inputs(&options, &jailer)?;
    raise_open_file_limit();
    fs::create_dir_all(&options.data_dir)?;
    fs::set_permissions(&options.data_dir, fs::Permissions::from_mode(0o700))?;
    let jails = options.data_dir.join("jails");
    let _ = fs::remove_dir_all(&jails);
    fs::create_dir_all(&jails)?;
    let cgroups = if options.no_jail {
        tracing::warn!("running VMMs without the jail and cgroups");
        None
    } else {
        let root = CgroupRoot::prepare(&options.cgroup_root).with_context(|| {
            format!(
                "cannot set up cgroups under {}",
                options.cgroup_root.display()
            )
        })?;
        let removed = root.remove_leftovers();
        if removed > 0 {
            tracing::warn!("removed {removed} cgroups left by an earlier run");
        }
        Some(root)
    };
    let jail = JailConfig {
        enabled: !options.no_jail,
        jailer,
        vmm: options.vmm.clone(),
        base_dir: jails.clone(),
    };
    let launcher = Arc::new(Launcher::new(
        jail,
        cgroups.clone(),
        options.vm_overhead_mib,
    ));
    let images = Images {
        kernel: options.kernel.clone(),
        runtime_dir: options.runtime_dir.clone(),
    };
    let builder = Builder::new(
        launcher.clone(),
        images,
        &options.data_dir,
        options.uid_base,
        options.init_timeout,
    );
    let limits = admission_limits(&options);
    tracing::info!(
        memory_limit_mib = limits.memory_limit_mib,
        vcpu_limit = limits.vcpu_limit,
        "admission limits"
    );
    let admission = Admission::new(limits, Box::new(|| meminfo_mib("MemAvailable")));
    let store = Store::open(&options.data_dir)?;
    let settings = Settings {
        uid_base: options.uid_base,
        idle_timeout: options.idle_timeout,
        queue_timeout: options.queue_timeout,
        vm_overhead_mib: options.vm_overhead_mib,
    };
    let platform = Platform::new(
        settings,
        store,
        launcher,
        builder,
        admission,
        Arc::new(Metrics::new()),
    );
    platform.recover().await?;
    platform.spawn_reaper();

    let listener = tokio::net::TcpListener::bind(options.listen)
        .await
        .with_context(|| format!("cannot listen on {}", options.listen))?;
    tracing::info!("listening on {}", options.listen);
    let app = api::router(
        platform.clone(),
        options.api_token.clone(),
        options.max_code_bytes,
    );
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
            tracing::info!("shutting down");
        })
        .await?;
    platform.shutdown().await;
    if let Some(cgroups) = &cgroups {
        cgroups.remove_leftovers();
    }
    let _ = fs::remove_dir_all(&jails);
    tracing::info!("stopped");
    Ok(())
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let options = Options::parse();
    init_tracing(options.log_format);
    match run(options).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
