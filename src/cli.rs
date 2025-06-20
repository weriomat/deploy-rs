// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
// SPDX-FileCopyrightText: 2021 Yannik Sander <contact@ysndr.de>
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::HashMap;
use std::io::{stdin, stdout, Write};
use std::str::Utf8Error;

use clap::{ArgMatches, Parser, FromArgMatches};

use crate as deploy;

use self::deploy::{DeployFlake, ParseFlakeError};
use futures_util::stream::{StreamExt, TryStreamExt};
use log::{debug, error, info, warn};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Stdio;
use thiserror::Error;
use tokio::fs::try_exists;
use tokio::process::Command;

/// Simple Rust rewrite of a simple Nix Flake deployment tool
#[derive(Parser, Debug, Clone)]
#[command(version = "1.0", author = "Serokell <https://serokell.io/>")]
pub struct Opts {
    /// The flake to deploy
    #[arg(group = "deploy")]
    target: Option<String>,

    /// A list of flakes to deploy alternatively
    #[arg(long, group = "deploy", num_args = 1..)]
    targets: Option<Vec<String>>,
    /// Treat targets as files instead of flakes
    #[clap(short, long)]
    file: Option<String>,
    /// Check signatures when using `nix copy`
    #[arg(short, long)]
    checksigs: bool,
    /// Use the interactive prompt before deployment
    #[arg(short, long)]
    interactive: bool,
    /// Extra arguments to be passed to nix build
    extra_build_args: Vec<String>,

    /// Print debug logs to output
    #[arg(short, long)]
    debug_logs: bool,
    /// Directory to print logs to (including the background activation process)
    #[arg(long)]
    log_dir: Option<String>,

    /// Keep the build outputs of each built profile
    #[arg(short, long)]
    keep_result: bool,
    /// Location to keep outputs from built profiles in
    #[arg(short, long)]
    result_path: Option<String>,

    /// Skip the automatic pre-build checks
    #[arg(short, long)]
    skip_checks: bool,

    /// Build on remote host
    #[arg(long)]
    remote_build: bool,

    /// Override the SSH user with the given value
    #[arg(long)]
    ssh_user: Option<String>,
    /// Override the profile user with the given value
    #[arg(long)]
    profile_user: Option<String>,
    /// Override the SSH options used
    #[arg(long, allow_hyphen_values = true)]
    ssh_opts: Option<String>,
    /// Override the SSH compression when using `nix copy`
    #[clap(long)]
    compress: Option<bool>,
    /// Override if the connecting to the target node should be considered fast
    #[arg(long)]
    fast_connection: Option<bool>,
    /// Override if a rollback should be attempted if activation fails
    #[arg(long)]
    auto_rollback: Option<bool>,
    /// Override hostname used for the node
    #[arg(long)]
    hostname: Option<String>,
    /// Make activation wait for confirmation, or roll back after a period of time
    #[arg(long)]
    magic_rollback: Option<bool>,
    /// How long activation should wait for confirmation (if using magic-rollback)
    #[arg(long)]
    confirm_timeout: Option<u16>,
    /// How long we should wait for profile activation
    #[arg(long)]
    activation_timeout: Option<u16>,
    /// Where to store temporary files (only used by magic-rollback)
    #[arg(long)]
    temp_path: Option<PathBuf>,
    /// Show what will be activated on the machines
    #[arg(long)]
    dry_activate: bool,
    /// Don't activate, but update the boot loader to boot into the new profile
    #[arg(long)]
    boot: bool,
    /// Revoke all previously succeeded deploys when deploying multiple profiles
    #[arg(long)]
    rollback_succeeded: Option<bool>,
    /// Which sudo command to use. Must accept at least two arguments: user name to execute commands as and the rest is the command to execute
    #[arg(long)]
    sudo: Option<String>,
    /// Prompt for sudo password during activation.
    #[arg(long)]
    interactive_sudo: Option<bool>,
    /// File for the sudo password with sops integration
    #[arg(long)]
    sudo_file: Option<PathBuf>,
    /// Key for the sudo password with sops integration
    #[arg(long)]
    sudo_secret: Option<String>,
}

/// Returns if the available Nix installation supports flakes
async fn test_flake_support() -> Result<bool, std::io::Error> {
    debug!("Checking for flake support");

    Ok(Command::new("nix")
        .arg("eval")
        .arg("--expr")
        .arg("builtins.getFlake")
        // This will error on some machines "intentionally", and we don't really need that printing
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?
        .success())
}

#[derive(Error, Debug)]
pub enum CheckDeploymentError {
    #[error("Failed to execute Nix checking command: {0}")]
    NixCheck(#[from] std::io::Error),
    #[error("Nix checking command resulted in a bad exit code: {0:?}")]
    NixCheckExit(Option<i32>),
}

async fn check_deployment(
    supports_flakes: bool,
    repo: &str,
    extra_build_args: &[String],
) -> Result<(), CheckDeploymentError> {
    info!("Running checks for flake in {}", repo);

    let mut check_command = match supports_flakes {
        true => Command::new("nix"),
        false => Command::new("nix-build"),
    };

    if supports_flakes {
        check_command.arg("flake").arg("check").arg(repo);
    } else {
        check_command.arg("-E")
                .arg("--no-out-link")
                .arg(format!("let r = import {}/.; x = (if builtins.isFunction r then (r {{}}) else r); in if x ? checks then x.checks.${{builtins.currentSystem}} else {{}}", repo));
    }

    check_command.args(extra_build_args);

    let check_status = check_command.status().await?;

    match check_status.code() {
        Some(0) => (),
        a => return Err(CheckDeploymentError::NixCheckExit(a)),
    };

    Ok(())
}

#[derive(Error, Debug)]
pub enum GetDeploymentDataError {
    #[error("Failed to execute nix eval command: {0}")]
    NixEval(std::io::Error),
    #[error("Failed to read output from evaluation: {0}")]
    NixEvalOut(std::io::Error),
    #[error("Evaluation resulted in a bad exit code: {0:?}")]
    NixEvalExit(Option<i32>),
    #[error("Error converting evaluation output to utf8: {0}")]
    DecodeUtf8(#[from] std::string::FromUtf8Error),
    #[error("Error decoding the JSON from evaluation: {0}")]
    DecodeJson(#[from] serde_json::error::Error),
    #[error("Impossible happened: profile is set but node is not")]
    ProfileNoNode,
}

/// Evaluates the Nix in the given `repo` and return the processed Data from it
async fn get_deployment_data(
    supports_flakes: bool,
    flakes: &[deploy::DeployFlake<'_>],
    extra_build_args: &[String],
) -> Result<Vec<deploy::data::Data>, GetDeploymentDataError> {
    futures_util::stream::iter(flakes).then(|flake| async move {

    info!("Evaluating flake in {}", flake.repo);

    let mut c = if supports_flakes {
        Command::new("nix")
    } else {
        Command::new("nix-instantiate")
    };

    if supports_flakes {
        c.arg("eval")
            .arg("--json")
            .arg(format!("{}#deploy", flake.repo))
            // We use --apply instead of --expr so that we don't have to deal with builtins.getFlake
            .arg("--apply");
        match (&flake.node, &flake.profile) {
            (Some(node), Some(profile)) => {
                // Ignore all nodes and all profiles but the one we're evaluating
                c.arg(format!(
                    r#"
                      deploy:
                      (deploy // {{
                        nodes = {{
                          "{0}" = deploy.nodes."{0}" // {{
                            profiles = {{
                              inherit (deploy.nodes."{0}".profiles) "{1}";
                            }};
                          }};
                        }};
                      }})
                     "#,
                    node, profile
                ))
            }
            (Some(node), None) => {
                // Ignore all nodes but the one we're evaluating
                c.arg(format!(
                    r#"
                      deploy:
                      (deploy // {{
                        nodes = {{
                          inherit (deploy.nodes) "{}";
                        }};
                      }})
                    "#,
                    node
                ))
            }
            (None, None) => {
                // We need to evaluate all profiles of all nodes anyway, so just do it strictly
                c.arg("deploy: deploy")
            }
            (None, Some(_)) => return Err(GetDeploymentDataError::ProfileNoNode),
        }
    } else {
        c
            .arg("--strict")
            .arg("--read-write-mode")
            .arg("--json")
            .arg("--eval")
            .arg("-E")
            .arg(format!("let r = import {}/.; in if builtins.isFunction r then (r {{}}).deploy else r.deploy", flake.repo))
    };

    c.args(extra_build_args);

    let build_child = c
        .stdout(Stdio::piped())
        .spawn()
        .map_err(GetDeploymentDataError::NixEval)?;

    let build_output = build_child
        .wait_with_output()
        .await
        .map_err(GetDeploymentDataError::NixEvalOut)?;

    match build_output.status.code() {
        Some(0) => (),
        a => return Err(GetDeploymentDataError::NixEvalExit(a)),
    };

    let data_json = String::from_utf8(build_output.stdout)?;

    Ok(serde_json::from_str(&data_json)?)
}).try_collect().await
}

#[derive(Serialize)]
struct PromptPart<'a> {
    user: &'a str,
    ssh_user: &'a str,
    path: &'a str,
    hostname: &'a str,
    ssh_opts: &'a [String],
}

fn print_deployment(
    parts: &[(
        &deploy::DeployFlake<'_>,
        deploy::DeployData,
        deploy::DeployDefs,
    )],
) -> Result<(), toml::ser::Error> {
    let mut part_map: HashMap<String, HashMap<String, PromptPart>> = HashMap::new();

    for (_, data, defs) in parts {
        part_map
            .entry(data.node_name.to_string())
            .or_insert_with(HashMap::new)
            .insert(
                data.profile_name.to_string(),
                PromptPart {
                    user: &defs.profile_user,
                    ssh_user: &defs.ssh_user,
                    path: &data.profile.profile_settings.path,
                    hostname: &data.node.node_settings.hostname,
                    ssh_opts: &data.merged_settings.ssh_opts,
                },
            );
    }

    let toml = toml::to_string(&part_map)?;

    info!("The following profiles are going to be deployed:\n{}", toml);

    Ok(())
}
#[derive(Error, Debug)]
pub enum PromptDeploymentError {
    #[error("Failed to make printable TOML of deployment: {0}")]
    TomlFormat(#[from] toml::ser::Error),
    #[error("Failed to flush stdout prior to query: {0}")]
    StdoutFlush(std::io::Error),
    #[error("Failed to read line from stdin: {0}")]
    StdinRead(std::io::Error),
    #[error("User cancelled deployment")]
    Cancelled,
}

fn prompt_deployment(
    parts: &[(
        &deploy::DeployFlake<'_>,
        deploy::DeployData,
        deploy::DeployDefs,
    )],
) -> Result<(), PromptDeploymentError> {
    print_deployment(parts)?;

    info!("Are you sure you want to deploy these profiles?");
    print!("> ");

    stdout()
        .flush()
        .map_err(PromptDeploymentError::StdoutFlush)?;

    let mut s = String::new();
    stdin()
        .read_line(&mut s)
        .map_err(PromptDeploymentError::StdinRead)?;

    if !yn::yes(&s) {
        if yn::is_somewhat_yes(&s) {
            info!("Sounds like you might want to continue, to be more clear please just say \"yes\". Do you want to deploy these profiles?");
            print!("> ");

            stdout()
                .flush()
                .map_err(PromptDeploymentError::StdoutFlush)?;

            let mut s = String::new();
            stdin()
                .read_line(&mut s)
                .map_err(PromptDeploymentError::StdinRead)?;

            if !yn::yes(&s) {
                return Err(PromptDeploymentError::Cancelled);
            }
        } else {
            if !yn::no(&s) {
                info!(
                    "That was unclear, but sounded like a no to me. Please say \"yes\" or \"no\" to be more clear."
                );
            }

            return Err(PromptDeploymentError::Cancelled);
        }
    }

    Ok(())
}

#[derive(Error, Debug)]
pub enum RunDeployError {
    #[error("Failed to deploy profile to node {0}: {1}")]
    DeployProfile(String, deploy::deploy::DeployProfileError),
    #[error("Failed to build profile on node {0}: {0}")]
    BuildProfile(String,  deploy::push::PushProfileError),
    #[error("Failed to push profile to node {0}: {0}")]
    PushProfile(String,  deploy::push::PushProfileError),
    #[error("No profile named `{0}` was found")]
    ProfileNotFound(String),
    #[error("No node named `{0}` was found")]
    NodeNotFound(String),
    #[error("Profile was provided without a node name")]
    ProfileWithoutNode,
    #[error("Error processing deployment definitions: {0}")]
    DeployDataDefs(#[from] deploy::DeployDataDefsError),
    #[error("Failed to make printable TOML of deployment: {0}")]
    TomlFormat(#[from] toml::ser::Error),
    #[error("{0}")]
    PromptDeployment(#[from] PromptDeploymentError),
    #[error("Failed to revoke profile for node {0}: {1}")]
    RevokeProfile(String, deploy::deploy::RevokeProfileError),
    #[error("Deployment to node {0} failed, rolled back to previous generation")]
    Rollback(String),
    #[error("Failed to get the password from sops: {0}")]
    Sops(#[from] deploy::cli::SopsError),
}

type ToDeploy<'a> = Vec<(
    &'a deploy::DeployFlake<'a>,
    &'a deploy::data::Data,
    (&'a str, &'a deploy::data::Node),
    (&'a str, &'a deploy::data::Profile),
)>;

async fn run_deploy(
    deploy_flakes: Vec<deploy::DeployFlake<'_>>,
    data: Vec<deploy::data::Data>,
    supports_flakes: bool,
    check_sigs: bool,
    interactive: bool,
    cmd_overrides: &deploy::CmdOverrides,
    keep_result: bool,
    result_path: Option<&str>,
    extra_build_args: &[String],
    debug_logs: bool,
    dry_activate: bool,
    boot: bool,
    log_dir: &Option<String>,
    rollback_succeeded: bool,
) -> Result<(), RunDeployError> {
    let to_deploy: ToDeploy = deploy_flakes
        .iter()
        .zip(&data)
        .map(|(deploy_flake, data)| {
            let to_deploys: ToDeploy = match (&deploy_flake.node, &deploy_flake.profile) {
                (Some(node_name), Some(profile_name)) => {
                    let node = match data.nodes.get(node_name) {
                        Some(x) => x,
                        None => return Err(RunDeployError::NodeNotFound(node_name.clone())),
                    };
                    let profile = match node.node_settings.profiles.get(profile_name) {
                        Some(x) => x,
                        None => return Err(RunDeployError::ProfileNotFound(profile_name.clone())),
                    };

                    vec![(
                        deploy_flake,
                        data,
                        (node_name.as_str(), node),
                        (profile_name.as_str(), profile),
                    )]
                }
                (Some(node_name), None) => {
                    let node = match data.nodes.get(node_name) {
                        Some(x) => x,
                        None => return Err(RunDeployError::NodeNotFound(node_name.clone())),
                    };

                    let mut profiles_list: Vec<(&str, &deploy::data::Profile)> = Vec::new();

                    for profile_name in [
                        node.node_settings.profiles_order.iter().collect(),
                        node.node_settings.profiles.keys().collect::<Vec<&String>>(),
                    ]
                    .concat()
                    {
                        let profile = match node.node_settings.profiles.get(profile_name) {
                            Some(x) => x,
                            None => {
                                return Err(RunDeployError::ProfileNotFound(profile_name.clone()))
                            }
                        };

                        if !profiles_list.iter().any(|(n, _)| n == profile_name) {
                            profiles_list.push((profile_name, profile));
                        }
                    }

                    profiles_list
                        .into_iter()
                        .map(|x| (deploy_flake, data, (node_name.as_str(), node), x))
                        .collect()
                }
                (None, None) => {
                    let mut l = Vec::new();

                    for (node_name, node) in &data.nodes {
                        let mut profiles_list: Vec<(&str, &deploy::data::Profile)> = Vec::new();

                        for profile_name in [
                            node.node_settings.profiles_order.iter().collect(),
                            node.node_settings.profiles.keys().collect::<Vec<&String>>(),
                        ]
                        .concat()
                        {
                            let profile = match node.node_settings.profiles.get(profile_name) {
                                Some(x) => x,
                                None => {
                                    return Err(RunDeployError::ProfileNotFound(
                                        profile_name.clone(),
                                    ))
                                }
                            };

                            if !profiles_list.iter().any(|(n, _)| n == profile_name) {
                                profiles_list.push((profile_name, profile));
                            }
                        }

                        let ll: ToDeploy = profiles_list
                            .into_iter()
                            .map(|x| (deploy_flake, data, (node_name.as_str(), node), x))
                            .collect();

                        l.extend(ll);
                    }

                    l
                }
                (None, Some(_)) => return Err(RunDeployError::ProfileWithoutNode),
            };
            Ok(to_deploys)
        })
        .collect::<Result<Vec<ToDeploy>, RunDeployError>>()?
        .into_iter()
        .flatten()
        .collect();

    let mut parts: Vec<(
        &deploy::DeployFlake<'_>,
        deploy::DeployData,
        deploy::DeployDefs,
    )> = Vec::new();

    for (deploy_flake, data, (node_name, node), (profile_name, profile)) in to_deploy {
        let deploy_data = deploy::make_deploy_data(
            &data.generic_settings,
            node,
            node_name,
            profile,
            profile_name,
            cmd_overrides,
            debug_logs,
            log_dir.as_deref(),
        );

        let mut deploy_defs = deploy_data.defs()?;

        if deploy_data.merged_settings.sudo.is_some()
            && (deploy_data.merged_settings.interactive_sudo.is_some()
                || deploy_data.merged_settings.sudo_secret.is_some())
        {
            warn!("Custom sudo commands should be configured to accept password input from stdin when using the 'interactive sudo' or 'password File' option. Deployment may fail if the custom command ignores stdin.");
        } else {
            // this configures sudo to hide the password prompt and accept input from stdin
            // at the time of writing, deploy_defs.sudo defaults to 'sudo -u root' when using user=root and sshUser as non-root
            let original = deploy_defs.sudo.unwrap_or("sudo".to_string());
            deploy_defs.sudo = Some(format!("{} -S -p \"\"", original));
        }

        if deploy_data
            .merged_settings
            .interactive_sudo
            .unwrap_or(false)
        {
            warn!("Interactive sudo is enabled! Using a sudo password is less secure than correctly configured SSH keys.\nPlease use keys in production environments.");

            info!(
                "You will now be prompted for the sudo password for {}.",
                node.node_settings.hostname
            );

            let sudo_password = rpassword::prompt_password(format!(
                "(sudo for {}) Password: ",
                node.node_settings.hostname
            ))
            .unwrap_or("".to_string());

            deploy_defs.sudo_password = Some(sudo_password);
        } else if deploy_data.merged_settings.sudo_file.is_some()
            && deploy_data.merged_settings.sudo_secret.is_some()
        {
            // SAFETY: we already checked if it is some
            let path = deploy_data.merged_settings.sudo_file.clone().unwrap();
            let key = deploy_data.merged_settings.sudo_secret.clone().unwrap();

            if !try_exists(&path).await.unwrap() {
                return Err(RunDeployError::Sops(SopsError::SopsFileNotFound(format!(
                    "{path:?} not found"
                ))));
            }

            // We deserialze to json
            let out = Command::new("sops")
                .arg("--output-type")
                .arg("json")
                .arg("-d")
                .arg(&path)
                .output()
                .await
                .map_err(|err| {
                    RunDeployError::Sops(SopsError::SopsFailedDecryption(
                        path.to_string_lossy().into(),
                        err,
                    ))
                })?;

            let conv_out = std::str::from_utf8(&out.stdout)
                .map_err(|err| RunDeployError::Sops(SopsError::SopsCannotConvert(err)))?;

            let mut m: serde_json::Map<String, serde_json::Value> = serde_json::from_str(conv_out)
                .map_err(|err| RunDeployError::Sops(SopsError::SerdeDeserialize(err)))?;

            let mut sudo_password = String::new();

            // We support nested keys like a/b/c
            for i in key.split('/') {
                match m.get(i) {
                    Some(v) => match v {
                        serde_json::Value::String(s) => {
                            sudo_password = s.into();
                        }
                        serde_json::Value::Bool(b) => {
                            sudo_password = b.to_string();
                        }
                        serde_json::Value::Number(n) => {
                            sudo_password = n.to_string();
                        }
                        serde_json::Value::Object(map) => {
                            m = map.clone();
                        }
                        _ => {
                            return Err(RunDeployError::Sops(SopsError::SerdeUnexpectedType(
                                "We dont handle Arrays, Bools, None, Numbers".into(),
                            )));
                        }
                    },
                    None => {
                        return Err(RunDeployError::Sops(SopsError::SopsKeyNotFound(format!(
                            "Did not find {} in Map",
                            i
                        ))));
                    }
                }
            }
            deploy_defs.sudo_password = Some(sudo_password);
        }

        parts.push((deploy_flake, deploy_data, deploy_defs));
    }

    if interactive {
        prompt_deployment(&parts[..])?;
    } else {
        print_deployment(&parts[..])?;
    }

    let data_iter = || {
        parts.iter().map(
            |(deploy_flake, deploy_data, deploy_defs)| deploy::push::PushProfileData {
                supports_flakes,
                check_sigs,
                repo: deploy_flake.repo,
                deploy_data,
                deploy_defs,
                keep_result,
                result_path,
                extra_build_args,
            },
        )
    };

    for data in data_iter() {
        let node_name: String = data.deploy_data.node_name.to_string();
        deploy::push::build_profile(data).await.map_err(|e| {
            RunDeployError::BuildProfile(node_name, e)
        })?;
    }

    for data in data_iter() {
        let node_name: String = data.deploy_data.node_name.to_string();
        deploy::push::push_profile(data).await.map_err(|e| {
            RunDeployError::PushProfile(node_name, e)
        })?;
    }

    let mut succeeded: Vec<(&deploy::DeployData, &deploy::DeployDefs)> = vec![];

    // Run all deployments
    // In case of an error rollback any previoulsy made deployment.
    // Rollbacks adhere to the global seeting to auto_rollback and secondary
    // the profile's configuration
    for (_, deploy_data, deploy_defs) in &parts {
        if let Err(e) = deploy::deploy::deploy_profile(deploy_data, deploy_defs, dry_activate, boot).await
        {
            error!("{}", e);
            if dry_activate {
                info!("dry run, not rolling back");
            }
            if rollback_succeeded && cmd_overrides.auto_rollback.unwrap_or(true) {
                info!("Revoking previous deploys");
                // revoking all previous deploys
                // (adheres to profile configuration if not set explicitely by
                //  the command line)
                for (deploy_data, deploy_defs) in &succeeded {
                    if deploy_data.merged_settings.auto_rollback.unwrap_or(true) {
                        deploy::deploy::revoke(*deploy_data, *deploy_defs).await.map_err(|e| {
                            RunDeployError::RevokeProfile(deploy_data.node_name.to_string(), e)
                        })?;
                    }
                }
                return Err(RunDeployError::Rollback(deploy_data.node_name.to_string()));
            }
            return Err(RunDeployError::DeployProfile(deploy_data.node_name.to_string(), e))
        }
        succeeded.push((deploy_data, deploy_defs))
    }

    Ok(())
}

#[derive(Error, Debug)]
pub enum SopsError {
    #[error("Failed to decrypt file {0}: {1}")]
    SopsFailedDecryption(String, std::io::Error),
    #[error("Failed to find sops file: {0}")]
    SopsFileNotFound(String),
    #[error("Failed to convert the output of sops to a str: {0}")]
    SopsCannotConvert(Utf8Error),
    #[error("Failed to deserialize: {0}")]
    SerdeDeserialize(serde_json::Error),
    #[error("Error unexpected type: {0}")]
    SerdeUnexpectedType(String),
    #[error("Failed to find key: {0}")]
    SopsKeyNotFound(String),
}

#[derive(Error, Debug)]
pub enum RunError {
    #[error("Failed to deploy profile: {0}")]
    DeployProfile(#[from] deploy::deploy::DeployProfileError),
    #[error("Failed to push profile: {0}")]
    PushProfile(#[from] deploy::push::PushProfileError),
    #[error("Failed to test for flake support: {0}")]
    FlakeTest(std::io::Error),
    #[error("Failed to check deployment: {0}")]
    CheckDeployment(#[from] CheckDeploymentError),
    #[error("Failed to evaluate deployment data: {0}")]
    GetDeploymentData(#[from] GetDeploymentDataError),
    #[error("Error parsing flake: {0}")]
    ParseFlake(#[from] deploy::ParseFlakeError),
    #[error("Error parsing arguments: {0}")]
    ParseArgs(#[from] clap::Error),
    #[error("Error initiating logger: {0}")]
    Logger(#[from] flexi_logger::FlexiLoggerError),
    #[error("{0}")]
    RunDeploy(#[from] RunDeployError),
}

pub async fn run(args: Option<&ArgMatches>) -> Result<(), RunError> {
    let opts = match args {
        Some(o) => <Opts as FromArgMatches>::from_arg_matches(o)?,
        None => Opts::parse(),
    };

    deploy::init_logger(
        opts.debug_logs,
        opts.log_dir.as_deref(),
        &deploy::LoggerType::Deploy,
    )?;

    if opts.dry_activate && opts.boot {
        error!("Cannot use both --dry-activate & --boot!");
    }

    let deploys = opts
        .clone()
        .targets
        .unwrap_or_else(|| vec![opts.clone().target.unwrap_or_else(|| ".".to_string())]);

    let deploy_flakes: Vec<DeployFlake> =
        if let Some(file) = &opts.file {
            deploys
                .iter()
                .map(|f| deploy::parse_file(file.as_str(), f.as_str()))
                .collect::<Result<Vec<DeployFlake>, ParseFlakeError>>()?
        }
    else {
        deploys
        .iter()
        .map(|f| deploy::parse_flake(f.as_str()))
          .collect::<Result<Vec<DeployFlake>, ParseFlakeError>>()?
    };

    let cmd_overrides = deploy::CmdOverrides {
        ssh_user: opts.ssh_user,
        profile_user: opts.profile_user,
        ssh_opts: opts.ssh_opts,
        fast_connection: opts.fast_connection,
        compress: opts.compress,
        auto_rollback: opts.auto_rollback,
        hostname: opts.hostname,
        magic_rollback: opts.magic_rollback,
        temp_path: opts.temp_path,
        confirm_timeout: opts.confirm_timeout,
        activation_timeout: opts.activation_timeout,
        dry_activate: opts.dry_activate,
        remote_build: opts.remote_build,
        sudo: opts.sudo,
        interactive_sudo: opts.interactive_sudo,
        sudo_file: opts.sudo_file,
        sudo_secret: opts.sudo_secret,
    };

    let supports_flakes = test_flake_support().await.map_err(RunError::FlakeTest)?;
    let do_not_want_flakes = opts.file.is_some();

    if !supports_flakes {
        warn!("A Nix version without flakes support was detected, support for this is work in progress");
    }

    if do_not_want_flakes {
        warn!("The --file option for deployments without flakes is experimental");
    }

    let using_flakes = supports_flakes && !do_not_want_flakes;

    if !opts.skip_checks {
        let mut set = std::collections::HashSet::new();
        deploy_flakes.iter().for_each(|item| {
            set.insert(item.repo);
        });

        for path in set {
            check_deployment(using_flakes, path, &opts.extra_build_args).await?;
        }
    }
    let result_path = opts.result_path.as_deref();
    let data = get_deployment_data(using_flakes, &deploy_flakes, &opts.extra_build_args).await?;
    run_deploy(
        deploy_flakes,
        data,
        using_flakes,
        opts.checksigs,
        opts.interactive,
        &cmd_overrides,
        opts.keep_result,
        result_path,
        &opts.extra_build_args,
        opts.debug_logs,
        opts.dry_activate,
        opts.boot,
        &opts.log_dir,
        opts.rollback_succeeded.unwrap_or(true),
    )
    .await?;

    Ok(())
}
