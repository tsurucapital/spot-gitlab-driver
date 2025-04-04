use aws_sdk_ec2::types::{
    FleetLaunchTemplateSpecification, LaunchTemplateOverrides, SpotFleetRequestConfigData,
};
use bpaf::{Bpaf, Parser};
use std::path::{Path, PathBuf};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Clone, Bpaf)]
struct Opts {
    /// Force tracing output to journald?
    journald: bool,
    #[bpaf(external)]
    cmd: Cmd,
}

#[derive(Debug, Clone, Bpaf)]
enum Cmd {
    #[bpaf(command)]
    Config,
    // Custom,
    #[bpaf(command)]
    Prepare,
    #[bpaf(command)]
    Run {
        #[bpaf(positional)]
        script_path: PathBuf,
        #[bpaf(positional)]
        script_name: String,
    },
    #[bpaf(command)]
    Cleanup,
}

#[tokio::main]
async fn main() {
    let Opts { journald, cmd } = opts().to_options().run();
    logger(journald).unwrap();

    let job_id =
        std::env::var("CUSTOM_ENV_CI_JOB_ID").expect("CUSTOM_ENV_CI_JOB_ID env var should be set");
    let job_id_span = tracing::span!(tracing::Level::DEBUG, "job_id", job_id = %job_id);
    let _job_id_span = job_id_span.enter();

    match cmd {
        Cmd::Config => config().await,
        Cmd::Prepare => {
            if let Err(e) = prepare().await {
                tracing::error!(?e, "Failed to prepare");
                eprintln!("Failed to prepare: {:?}", e);
                if let Err(e) = cleanup().await {
                    eprintln!("Failed to cleanup: {:?}", e);
                    tracing::error!(?e, "Failed to cleanup");
                }
                build_failure();
            }
        }
        Cmd::Run {
            script_path,
            script_name,
        } => {
            if let Err(e) = run(&script_path, &script_name).await {
                tracing::error!("Failed to run script: {}", e);
                eprintln!("Failed to run script: {:?}", e);
                if let Err(e) = cleanup().await {
                    eprintln!("Failed to cleanup: {:?}", e);
                    tracing::error!(?e, "Failed to cleanup");
                }
                build_failure();
            }
        }
        Cmd::Cleanup => {
            if let Err(e) = cleanup().await {
                eprintln!("Failed to cleanup: {:?}", e);
                tracing::error!(?e, "Failed to cleanup");
                build_failure();
            }
        }
    }
}

#[tracing::instrument]
async fn config() {
    let Ok(project_name) = std::env::var("CUSTOM_ENV_CI_PROJECT_NAME") else {
        eprintln!("CUSTOM_ENV_CI_PROJECT_NAME env var not set");
        build_failure();
    };

    // If timeout is specified, set upper bound an the spot fleet request.
    let valid_until = std::env::var("CUSTOM_ENV_CI_JOB_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|timeout_sec| {
            let sec_since_epoch_with_timeout = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + timeout_sec;
            aws_sdk_ec2::primitives::DateTime::from_secs(sec_since_epoch_with_timeout as i64)
        });

    // Get the SSH private key part while we are waiting for an instance below
    // anyway. We made it with pulumi, see
    // https://www.pulumi.com/registry/packages/aws-native/api-docs/ec2/keypair/.
    let sdk = sdk_config().await;
    let ssm = aws_sdk_ssm::Client::new(&sdk);
    let ec2 = aws_sdk_ec2::Client::new(&sdk);

    let private_key = {
        let key_name = &format!("{project_name}-ci-build");
        tracing::debug!(?key_name, "Fetching SSH key pair for the builder");
        let describe_key_pairs_output =
            match ec2.describe_key_pairs().key_names(key_name).send().await {
                Ok(output) => output,
                Err(e) => {
                    eprintln!("Failed to get the key pair for {key_name}: {e:?}");
                    build_failure();
                }
            };
        tracing::debug!(
            ?describe_key_pairs_output,
            "Got describe_key_pairs response"
        );
        let Some(key_pair_id) = describe_key_pairs_output
            .key_pairs
            .and_then(|kps| kps.into_iter().next())
            .and_then(|kpi| kpi.key_pair_id)
        else {
            eprintln!("Failed to get the key pair ID for {key_name}...");
            build_failure();
        };

        let param_name = format!("/ec2/keypair/{key_pair_id}");
        tracing::debug!(
            ?key_pair_id,
            ?param_name,
            "Got key pair ID, fetching private key"
        );
        let get_parameter_output = match ssm
            .get_parameter()
            .name(&param_name)
            .with_decryption(true)
            .send()
            .await
        {
            Ok(output) => output,
            Err(e) => {
                eprintln!("Failed to get the private key from {param_name}: {e:?}");
                build_failure();
            }
        };
        tracing::debug!(?get_parameter_output, "Got get_parameter response");
        let Some(private_key) = get_parameter_output.parameter.and_then(|p| p.value) else {
            eprintln!("Failed to get the private key for {key_name}...");
            build_failure()
        };

        let pk_path = match std::env::current_dir() {
            Ok(current_dir) => current_dir.join(format!("{project_name}-ci-build.pem")),
            Err(e) => {
                eprintln!("Failed to get current directory: {e:?}");
                build_failure();
            }
        };

        let write_key = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .mode(0o600)
                .create_new(true)
                .open(&pk_path)
                .await?;
            tokio::io::copy(&mut private_key.as_bytes(), &mut file).await?;
            Ok::<_, anyhow::Error>(())
        };

        if let Err(e) = write_key.await {
            eprintln!(
                "Failed to write private key to {}: {e:?}",
                pk_path.display()
            );
            build_failure();
        };
        pk_path.to_path_buf()
    };

    let spot_fleet_request_config = SpotFleetRequestConfigData::builder()
        .allocation_strategy(aws_sdk_ec2::types::AllocationStrategy::PriceCapacityOptimized)
        .iam_fleet_role("arn:aws:iam::822646120884:role/aws-ec2-spot-fleet-tagging-role")
        .terminate_instances_with_expiration(true)
        .target_capacity_unit_type(aws_sdk_ec2::types::TargetCapacityUnitType::Units)
        .target_capacity(1)
        .launch_template_configs(
            aws_sdk_ec2::types::LaunchTemplateConfig::builder()
                .launch_template_specification(
                    FleetLaunchTemplateSpecification::builder()
                        .launch_template_name(format!("{project_name}-ci-build"))
                        .version("$Latest")
                        .build(),
                )
                .overrides(
                    // gitlab-task-ap-northeast-1a
                    LaunchTemplateOverrides::builder()
                        .subnet_id("subnet-0bd69be8de06cd8b9")
                        .build(),
                )
                .overrides(
                    // gitlab-task-ap-northeast-1c
                    LaunchTemplateOverrides::builder()
                        .subnet_id("subnet-0512f60bbf2682eb5")
                        .build(),
                )
                .overrides(
                    // gitlab-task-ap-northeast-1d
                    LaunchTemplateOverrides::builder()
                        .subnet_id("subnet-06ec89ad3fa53365f")
                        .build(),
                )
                .build(),
        )
        // We can set the request to be only valid as long as the job timeout
        // allows: this is a safety measure, if everything goes to hell at least
        // the instance will die soon anyway!
        .set_valid_until(valid_until)
        .build();

    let Some(spot_fleet_request_id) = ec2
        .request_spot_fleet()
        .spot_fleet_request_config(spot_fleet_request_config)
        .send()
        .await
        .unwrap()
        .spot_fleet_request_id
    else {
        tracing::error!("Failed to get spot fleet request ID");
        build_failure();
    };

    // Once we have the spot fleet request, write it out. No more
    // crashing/unwrap/panic from here! All the other stages are in Result and
    // we handle the errors properly now that we have a real resource to manage.
    let state = State {
        spot_fleet_request_id,
        private_key,
        instance_address: None,
    };
    // Well, one last unwrap: nothing knows about the ID unless we preserve it
    // somehow. If this fails, we're in a bad situation anyway.
    state.write().unwrap();
}

#[tracing::instrument]
async fn prepare() -> anyhow::Result<()> {
    let mut state = State::load()?;
    let State {
        spot_fleet_request_id,
        private_key,
        instance_address,
    } = &mut state;
    let sdk = sdk_config().await;
    let ec2 = aws_sdk_ec2::Client::new(&sdk);

    tracing::debug!(
        ?spot_fleet_request_id,
        "Waiting for an instance to become available"
    );
    eprintln!("Waiting for an instance to become available at {spot_fleet_request_id}...");
    let (instance_id, instance_type) = loop {
        tracing::debug!(?spot_fleet_request_id, "Asking about active instances...");
        let resp = ec2
            .describe_spot_fleet_instances()
            .spot_fleet_request_id(&*spot_fleet_request_id)
            .send()
            .await?;
        tracing::debug!(
            ?spot_fleet_request_id,
            ?resp,
            "Got describe_spot_fleet_instances response"
        );
        if let Some(v) = resp.active_instances.and_then(|v| {
            let aws_sdk_ec2::types::ActiveInstance {
                instance_id,
                instance_type,
                ..
            } = v.into_iter().next()?;
            Some((instance_id?, instance_type?))
        }) {
            break v;
        }

        // Wait a bit before asking again.
        tracing::debug!(
            ?spot_fleet_request_id,
            "No active instances yet, waiting..."
        );
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    };

    tracing::debug!(
        ?spot_fleet_request_id,
        ?instance_id,
        ?instance_type,
        "Got instance ID and type"
    );
    eprintln!("Spot fleet yielded instance {instance_id} ({instance_type})");

    let describe_instances = ec2
        .describe_instances()
        .filters(
            aws_sdk_ec2::types::Filter::builder()
                .name("instance-id")
                .values(&instance_id)
                .build(),
        )
        .send()
        .await?;
    tracing::debug!(?describe_instances, "Got describe_addresses response");
    let Some(private_ip) = describe_instances
        .reservations
        .and_then(|v| v.into_iter().next())
        .and_then(|v| v.instances?.into_iter().next())
        .and_then(|v| v.private_ip_address)
    else {
        anyhow::bail!("Failed to get private IP address for {instance_id}");
    };

    // Should we wait for the status to be Running? Unsure.
    tracing::debug!(?private_ip, "Got private IP address");
    eprintln!("Instance {instance_id} has IP {private_ip}");

    // Log in as root afterwards, makes it easy to run builds, setup nix etc.
    let session = loop {
        tracing::debug!(?private_key, ?private_ip, "Connecting to instance");
        match openssh::SessionBuilder::default()
            .keyfile(&*private_key)
            .known_hosts_check(openssh::KnownHosts::Accept)
            .connect(format!("ec2-user@{private_ip}"))
            .await
        {
            Ok(session) => {
                tracing::debug!(?private_ip, "Connected to instance");
                break session;
            }
            Err(openssh::Error::Connect(e)) => {
                tracing::warn!(?e, "Failed to connect to instance, retrying soon...");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let c = session
        .command("sudo")
        .arg("cp")
        .arg("/home/ec2-user/.ssh/authorized_keys")
        .arg("/root/.ssh/authorized_keys")
        .spawn()
        .await?
        .wait()
        .await?;
    if c.success() {
        tracing::debug!("Made it possible to log-in as root from now")
    } else {
        anyhow::bail!("Failed to copy over authorized_keys to root");
    }

    *instance_address = Some(private_ip);
    state.write()?;

    Ok(())
}

#[tracing::instrument]
async fn run(script_path: &Path, script_name: &str) -> anyhow::Result<()> {
    let State {
        private_key,
        instance_address,
        ..
    } = State::load()?;
    let Some(instance_address) = instance_address else {
        anyhow::bail!("No instance address found during run phase");
    };

    tracing::debug!(?private_key, ?instance_address, "Connecting to instance");
    let session = openssh::SessionBuilder::default()
        .keyfile(&*private_key)
        .known_hosts_check(openssh::KnownHosts::Accept)
        .connect(format!("root@{instance_address}"))
        .await?;

    tracing::debug!(?script_name, "Running script");
    // Test command
    let mut cmd = session
        .command("bash")
        .stdin(openssh::Stdio::piped())
        .spawn()
        .await?;
    let Some(mut stdin) = cmd.stdin().take() else {
        anyhow::bail!("Failed to get stdin for command");
    };

    tracing::debug!(?script_path, "Copying script to remote machine...");
    tokio::io::copy(&mut tokio::fs::File::open(script_path).await?, &mut stdin).await?;

    tracing::debug!(?script_path, "Waiting for process to finish...");
    let status = cmd.wait().await?;
    if let Some(code) = status.code() {
        std::process::exit(code);
    } else if !status.success() {
        build_failure();
    }
    Ok(())
}

#[tracing::instrument]
async fn cleanup() -> anyhow::Result<()> {
    let State {
        spot_fleet_request_id,
        ..
    } = State::load()?;

    tracing::debug!(?spot_fleet_request_id, "Cleaning up spot fleet request");
    eprintln!("Cleaning up spot fleet request {spot_fleet_request_id}...");
    let sdk = sdk_config().await;
    let ec2 = aws_sdk_ec2::Client::new(&sdk);
    let resp = ec2
        .cancel_spot_fleet_requests()
        .spot_fleet_request_ids(&spot_fleet_request_id)
        .terminate_instances(true)
        .send()
        .await?;

    if !resp
        .successful_fleet_requests()
        .iter()
        .filter_map(|c| c.spot_fleet_request_id.as_deref())
        .any(|id| id == spot_fleet_request_id)
    {
        anyhow::bail!("Failed to cancel spot fleet request {spot_fleet_request_id}: {resp:?}");
    };

    eprintln!("Spot fleet request {spot_fleet_request_id} cancelled");

    // No need to hold onto old state files by this point.
    State::clean()
}

#[tracing::instrument]
async fn sdk_config() -> aws_config::SdkConfig {
    aws_config::ConfigLoader::default()
        .behavior_version(aws_config::BehaviorVersion::latest())
        .load()
        .await
}

fn logger(journald: bool) -> std::io::Result<()> {
    let subscriber = tracing_subscriber::registry();

    // Get the level from RUST_LOG, or fall back to DEFAULT_ENV_FILTER
    let subscriber = subscriber.with(
        EnvFilter::builder()
            .with_default_directive(tracing::level_filters::LevelFilter::DEBUG.into())
            .from_env_lossy(),
    );

    let mut dynamic_layers = Vec::with_capacity(1);

    fn use_journald() -> std::io::Result<bool> {
        let Ok(x) = std::env::var("JOURNAL_STREAM") else {
            return Ok(false);
        };
        let (device, inode) = x
            .split_once(':')
            .ok_or_else(|| std::io::Error::other("JOURNAL_STREAM wasn't a colon-separated pair"))?;
        let device = device
            .parse::<rustix::fs::Dev>()
            .map_err(std::io::Error::other)?;
        let inode = inode.parse::<u64>().map_err(std::io::Error::other)?;
        let stat = rustix::fs::fstat(std::io::stderr())?;
        Ok(stat.st_dev == device && stat.st_ino == inode)
    }

    // If journald is enabled, only log to there: this usually means we're
    // running as part of systemd service and logging to stderr would just
    // duplicate the logs.
    use tracing_subscriber::Layer;
    if journald || use_journald()? {
        dynamic_layers.push(tracing_journald::layer()?.boxed())
    } else {
        // Write events to stderr
        let fmt_layer = tracing_subscriber::fmt::layer();
        dynamic_layers.push(fmt_layer.with_writer(std::io::stderr).boxed())
    }

    subscriber.with(dynamic_layers).init();
    Ok(())
}

// Exit with BUILD_FAILURE_EXIT_CODE if set, otherwise 1.
fn build_failure() -> ! {
    tracing::debug!("Exiting with build failure...");
    let Some(build_failure_exit_code) = std::env::var("BUILD_FAILURE_EXIT_CODE")
        .ok()
        .and_then(|v| v.parse().ok())
    else {
        tracing::warn!(
            "BUILD_FAILURE_EXIT_CODE env var not set/valid, set it to the exit code to use!"
        );
        std::process::exit(1);
    };

    std::process::exit(build_failure_exit_code);
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct State {
    spot_fleet_request_id: String,
    private_key: PathBuf,
    instance_address: Option<String>,
}

impl State {
    /// The state file location.
    #[tracing::instrument]
    fn location() -> anyhow::Result<PathBuf> {
        tracing::debug!("Getting state file location...");
        let job_id = std::env::var("CUSTOM_ENV_CI_JOB_ID")?;
        let current_dir = std::env::current_dir()?;
        Ok(current_dir
            .join(".spot-gitlab-driver")
            .join(format!("{job_id}.state")))
    }

    /// Load the state from the disk.
    #[tracing::instrument]
    fn load() -> anyhow::Result<Self> {
        let location = Self::location()?;
        tracing::debug!(?location, "Loading state from disk");
        Ok(serde_json::from_reader(std::fs::File::open(location)?)?)
    }

    /// Write the state to the disk.
    #[tracing::instrument]
    fn write(&self) -> anyhow::Result<()> {
        let location = Self::location()?;
        let Some(state_dir) = location.parent() else {
            return Err(anyhow::anyhow!(
                "Failed to get parent directory of {}",
                location.display()
            ));
        };
        std::fs::create_dir_all(state_dir)?;
        tracing::debug!(?location, "Writing state to disk");
        let mut file = std::fs::File::create(location)?;
        serde_json::to_writer_pretty(&mut file, &self)?;
        file.sync_all()?;
        Ok(())
    }

    /// Delete the state file.
    #[tracing::instrument]
    fn clean() -> anyhow::Result<()> {
        let location = Self::location()?;
        tracing::debug!(?location, "Cleaning up state file");
        Ok(std::fs::remove_file(location)?)
    }
}
