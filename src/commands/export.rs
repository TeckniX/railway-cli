use anyhow::Result;
use colored::*;
use is_terminal::IsTerminal;
use serde::Serialize;
use serde_json::Value;
use std::fmt::Display;
use std::fs;

use crate::{
    config::Configs,
    errors::RailwayError,
    util::prompt::{fake_select, prompt_options, prompt_options_skippable},
    workspace::{Project, Workspace, workspaces},
};

use super::*;

#[derive(Parser)]
pub struct Args {
    #[clap(long, short)]
    /// Output as JSON instead of TOML
    json: bool,

    #[clap(long, short)]
    /// Output file path (default: railway.toml or railway.json)
    output: Option<String>,

    #[clap(long, short)]
    /// Environment to export
    environment: Option<String>,

    #[clap(long, short, alias = "project_id")]
    /// Project to export
    project: Option<String>,

    #[clap(long, short)]
    /// Service to export
    service: Option<String>,

    #[clap(long, short)]
    /// Workspace to export from
    workspace: Option<String>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct RailwayToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<BuildConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deploy: Option<DeployConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environments: Option<std::collections::BTreeMap<String, EnvironmentConfig>>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct BuildConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    builder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nixpacks_plan: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nixpacks_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dockerfile_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nixpacks_config_path: Option<String>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct DeployConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    start_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pre_deploy_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_replicas: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    healthcheck_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    healthcheck_timeout: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    restart_policy_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    restart_policy_max_retries: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cron_schedule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sleep_application: Option<bool>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct EnvironmentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<BuildConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deploy: Option<DeployConfig>,
}

pub async fn command(args: Args) -> Result<()> {
    let configs = Configs::new()?;

    let linked_project = configs.get_linked_project().await.ok();

    let workspaces = workspaces().await?;
    let workspace = select_workspace_with_linked(
        args.workspace.clone(),
        args.project.clone(),
        linked_project.as_ref(),
        workspaces,
    )?;

    let project = select_project_with_linked(workspace, args.project.clone(), linked_project.as_ref())?;

    let environment = select_environment_with_linked(
        args.environment.clone(),
        &project,
        linked_project.as_ref(),
    )?;

    let service = select_service_with_linked(&project, &environment, args.service.clone(), linked_project.as_ref())?;

    let project_details = fetch_project_details(project.id()).await?;

    let service_config =
        extract_service_config(&project_details, &environment.id, service.as_ref());

    let toml = build_railway_toml(service_config?, &environment.name);

    let output_path = args.output.clone().unwrap_or_else(|| {
        if args.json {
            "railway.json".to_string()
        } else {
            "railway.toml".to_string()
        }
    });

    if args.json {
        let json = serde_json::to_string_pretty(&toml)?;
        fs::write(&output_path, json)?;
    } else {
        let toml_str = toml::to_string(&toml)?;
        fs::write(&output_path, toml_str)?;
    }

    println!(
        "{} {} saved to {}",
        "Configuration".green(),
        output_path.cyan().bold(),
        "successfully!".green()
    );

    Ok(())
}

async fn fetch_project_details(project_id: &str) -> Result<queries::project::ResponseData> {
    let configs = Configs::new()?;
    let vars = queries::project::Variables {
        id: project_id.to_string(),
    };
    let client = GQLClient::new_authorized(&configs)?;
    let response =
        post_graphql::<queries::Project, _>(&client, configs.get_backboard(), vars).await?;
    Ok(response)
}

fn extract_service_config(
    project: &queries::project::ResponseData,
    environment_id: &str,
    service: Option<&NormalisedService>,
) -> ServiceConfigResult {
    let project = &project.project;

    let env = match project
        .environments
        .edges
        .iter()
        .find(|e| e.node.id == environment_id)
    {
        Some(e) => &e.node,
        None => return Err(RailwayError::EnvironmentNotFound(environment_id.to_string()).into()),
    };

    let service_id = match service {
        Some(s) => &s.id,
        None => {
            if env.service_instances.edges.len() == 1 {
                &env.service_instances.edges[0].node.service_id
            } else {
                return Err(
                    RailwayError::ServiceNotFound("multiple services found".to_string()).into(),
                );
            }
        }
    };

    let si = match env
        .service_instances
        .edges
        .iter()
        .find(|si| si.node.service_id == *service_id)
    {
        Some(si) => &si.node,
        None => return Err(RailwayError::ServiceNotFound(service_id.to_string()).into()),
    };

    let latest_deployment = si.latest_deployment.as_ref();

    let meta = latest_deployment.and_then(|d| d.meta.as_ref());

    let get_meta_string = |key: &str| -> Option<String> {
        meta.and_then(|m| m.get(key).and_then(|v| v.as_str().map(String::from)))
    };

    let get_meta_value = |key: &str| -> Option<Value> { meta.and_then(|m| m.get(key).cloned()) };

    let get_meta_i64 = |key: &str| -> Option<i32> {
        meta.and_then(|m| m.get(key).and_then(|v| v.as_i64()))
            .map(|v| v as i32)
    };

    let get_meta_bool =
        |key: &str| -> Option<bool> { meta.and_then(|m| m.get(key).and_then(|v| v.as_bool())) };

    let build = BuildConfig {
        builder: None,
        build_command: None,
        nixpacks_plan: get_meta_value("buildCommand"),
        nixpacks_version: get_meta_string("nixpacksVersion"),
        dockerfile_path: get_meta_string("dockerfilePath"),
        nixpacks_config_path: get_meta_string("nixpacksConfigPath"),
    };

    let deploy = DeployConfig {
        start_command: si.start_command.clone(),
        pre_deploy_command: get_meta_string("preDeployCommand"),
        num_replicas: get_meta_i64("numReplicas"),
        healthcheck_path: get_meta_string("healthcheckPath"),
        healthcheck_timeout: get_meta_i64("healthcheckTimeout"),
        restart_policy_type: get_meta_string("restartPolicyType"),
        restart_policy_max_retries: get_meta_i64("restartPolicyMaxRetries"),
        cron_schedule: si.cron_schedule.clone(),
        region: get_meta_string("region"),
        runtime: get_meta_string("runtime"),
        sleep_application: get_meta_bool("sleepApplication"),
    };

    Ok((build, deploy))
}

type ServiceConfigResult = Result<(BuildConfig, DeployConfig)>;

fn build_railway_toml((build, deploy): (BuildConfig, DeployConfig), _env_name: &str) -> RailwayToml {
    let has_build =
        build.builder.is_some() || build.build_command.is_some() || build.nixpacks_plan.is_some();
    let has_deploy = deploy.start_command.is_some()
        || deploy.num_replicas.is_some()
        || deploy.healthcheck_path.is_some();

    let default_build = if has_build { Some(build) } else { None };

    let default_deploy = if has_deploy { Some(deploy) } else { None };

    RailwayToml {
        build: default_build,
        deploy: default_deploy,
        environments: None,
    }
}

fn select_service_with_linked(
    project: &NormalisedProject,
    environment: &NormalisedEnvironment,
    service: Option<String>,
    linked_project: Option<&crate::config::LinkedProject>,
) -> Result<Option<NormalisedService>, anyhow::Error> {
    let useful_services = project
        .services
        .iter()
        .filter(|&a| {
            a.service_instances
                .iter()
                .any(|instance| instance == &environment.id)
        })
        .cloned()
        .collect::<Vec<NormalisedService>>();

    if useful_services.is_empty() {
        return Ok(None);
    }

    let linked_service_id = linked_project
        .as_ref()
        .and_then(|lp| lp.service.as_deref());

    let linked_service = linked_service_id.and_then(|ls| {
            useful_services.iter().find(|s| {
                s.id.to_lowercase() == ls.to_lowercase()
                    || s.name.to_lowercase() == ls.to_lowercase()
            })
        });

    let service = if let Some(service) = service {
        let service_norm = useful_services.iter().find(|s| {
            (s.name.to_lowercase() == service.to_lowercase())
                || (s.id.to_lowercase() == service.to_lowercase())
        });
        if let Some(service) = service_norm {
            fake_select("Select a service", &service.name);
            Some(service.clone())
        } else {
            return Err(RailwayError::ServiceNotFound(service).into());
        }
    } else if let Some(ls) = linked_service {
        Some(ls.clone())
    } else if std::io::stdout().is_terminal() {
        prompt_options_skippable("Select a service <esc to skip>", useful_services)?
    } else {
        None
    };
    Ok(service)
}

fn select_environment_with_linked(
    environment: Option<String>,
    project: &NormalisedProject,
    linked_project: Option<&crate::config::LinkedProject>,
) -> Result<NormalisedEnvironment, anyhow::Error> {
    if project.environments.is_empty() {
        if project.has_restricted_environments {
            bail!("All environments in this project are restricted");
        } else {
            bail!("Project has no environments");
        }
    }

    let linked_env_id = linked_project
        .as_ref()
        .and_then(|lp| lp.environment.as_deref());

    let linked_env = linked_env_id.and_then(|le| {
        project.environments.iter().find(|e| {
            e.id.to_lowercase() == le.to_lowercase()
                || e.name.to_lowercase() == le.to_lowercase()
        })
        .cloned()
    });

    let environment = if let Some(environment) = environment {
        let env = project.environments.iter().find(|e| {
            (e.name.to_lowercase() == environment.to_lowercase())
                || (e.id.to_lowercase() == environment.to_lowercase())
        });
        if let Some(env) = env {
            fake_select("Select an environment", &env.name);
            env.clone()
        } else {
            return Err(RailwayError::EnvironmentNotFound(environment).into());
        }
    } else if let Some(le) = linked_env {
        le.clone()
    } else if project.environments.len() == 1 {
        let env = project.environments[0].clone();
        fake_select("Select an environment", &env.name);
        env
    } else {
        if !std::io::stdout().is_terminal() {
            bail!(
                "--environment required in non-interactive mode (multiple environments available)"
            );
        }
        prompt_options("Select an environment", project.environments.clone())?
    };
    Ok(environment)
}

fn select_project_with_linked(
    workspace: Workspace,
    project: Option<String>,
    linked_project: Option<&crate::config::LinkedProject>,
) -> Result<NormalisedProject, anyhow::Error> {
    let projects = workspace
        .projects()
        .into_iter()
        .filter(|p| p.deleted_at().is_none())
        .collect::<Vec<_>>();

    let linked_proj_id = linked_project
        .as_ref()
        .map(|lp| lp.project.as_str());

    let linked_proj = linked_proj_id.and_then(|lpproj| {
        projects.iter().find(|p| {
            p.id().to_lowercase() == lpproj.to_lowercase()
                || p.name().to_lowercase() == lpproj.to_lowercase()
        })
    })
    .cloned();

    let project = NormalisedProject::from({
        if let Some(project) = project {
            let proj = projects.into_iter().find(|pro| {
                (pro.id().to_lowercase() == project.to_lowercase())
                    || (pro.name().to_lowercase() == project.to_lowercase())
            });
            if let Some(project) = proj {
                fake_select("Select a project", &project.to_string());
                project
            } else {
                return Err(RailwayError::ProjectNotFoundInWorkspace(
                    project,
                    workspace.name().to_owned(),
                )
                .into());
            }
        } else if let Some(lp) = linked_proj {
            fake_select("Select a project", &lp.to_string());
            lp
        } else {
            prompt_workspace_projects(projects)?
        }
    });
    Ok(project)
}

fn select_workspace_with_linked(
    workspace_name: Option<String>,
    project: Option<String>,
    linked_project: Option<&crate::config::LinkedProject>,
    workspaces: Vec<Workspace>,
) -> Result<Workspace, anyhow::Error> {
    let workspace = match (project, workspace_name) {
        (Some(project), None) => {
            if let Some(workspace) = workspaces.iter().find(|w| {
                w.projects().iter().any(|pro| {
                    pro.id().to_lowercase() == project.to_lowercase()
                        || pro.name().to_lowercase() == project.to_lowercase()
                })
            }) {
                fake_select("Select a workspace", workspace.name());
                workspace.clone()
            } else {
                prompt_workspaces(workspaces)?
            }
        }
        (None, Some(workspace_arg)) | (Some(_), Some(workspace_arg)) => {
            if let Some(workspace) = workspaces.iter().find(|w| {
                w.id().to_lowercase() == workspace_arg.to_lowercase()
                    || w.team_id().map(str::to_lowercase) == Some(workspace_arg.to_lowercase())
                    || w.name().to_lowercase() == workspace_arg.to_lowercase()
            }) {
                fake_select("Select a workspace", workspace.name());
                workspace.clone()
            } else if workspace_arg.to_lowercase() == "personal" {
                bail!(RailwayError::NoPersonalWorkspace);
            } else {
                return Err(RailwayError::WorkspaceNotFound(workspace_arg.clone()).into());
            }
        }
        (None, None) => {
            if let Some(lp) = linked_project {
                if let Some(workspace) = workspaces.iter().find(|w| {
                    w.projects().iter().any(|pro| {
                        pro.id().to_lowercase() == lp.project.to_lowercase()
                            || pro.name().to_lowercase() == lp.name.as_ref().map(|n| n.to_lowercase()).unwrap_or_default()
                    })
                }) {
                    fake_select("Select a workspace", workspace.name());
                    return Ok(workspace.clone());
                }
            }
            prompt_workspaces(workspaces)?
        }
    };
    Ok(workspace)
}

fn prompt_workspaces(workspaces: Vec<Workspace>) -> Result<Workspace> {
    if workspaces.is_empty() {
        return Err(RailwayError::NoProjects.into());
    }
    if workspaces.len() == 1 {
        fake_select("Select a workspace", workspaces[0].name());
        return Ok(workspaces[0].clone());
    }
    if !std::io::stdout().is_terminal() {
        bail!("--workspace required in non-interactive mode (multiple workspaces available)");
    }
    prompt_options("Select a workspace", workspaces)
}

fn prompt_workspace_projects(projects: Vec<Project>) -> Result<Project, anyhow::Error> {
    if !std::io::stdout().is_terminal() {
        bail!("--project required in non-interactive mode");
    }
    prompt_options("Select a project", projects)
}

structstruck::strike! {
    #[strikethrough[derive(Debug, Clone, derive_new::new)]]
    #[allow(dead_code)]
    struct NormalisedProject {
        id: String,
        name: String,
        environments: Vec<struct NormalisedEnvironment {
            id: String,
            name: String
        }>,
        services: Vec<struct NormalisedService {
            id: String,
            name: String,
            service_instances: Vec<String>,
        }>,
        has_restricted_environments: bool,
    }
}

#[allow(dead_code)]
impl NormalisedProject {
    pub fn id(&self) -> &str {
        &self.id
    }
}

#[allow(dead_code)]
impl NormalisedService {
    pub fn id(&self) -> &str {
        &self.id
    }
}

macro_rules! build_service_env_map {
    ($environments:expr) => {{
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for env in $environments {
            for si in &env.node.service_instances.edges {
                map.entry(si.node.service_id.clone())
                    .or_default()
                    .push(env.node.id.clone());
            }
        }
        map
    }};
}

impl From<Project> for NormalisedProject {
    fn from(value: Project) -> Self {
        match value {
            Project::External(project) => {
                let total_envs = project.environments.edges.len();
                let mut service_env_map = build_service_env_map!(&project.environments.edges);
                let accessible_envs: Vec<_> = project
                    .environments
                    .edges
                    .into_iter()
                    .filter(|env| env.node.can_access)
                    .map(|env| NormalisedEnvironment::new(env.node.id, env.node.name))
                    .collect();
                let has_restricted = total_envs > accessible_envs.len();
                NormalisedProject::new(
                    project.id,
                    project.name,
                    accessible_envs,
                    project
                        .services
                        .edges
                        .into_iter()
                        .map(|service| {
                            let env_ids =
                                service_env_map.remove(&service.node.id).unwrap_or_default();
                            NormalisedService::new(service.node.id, service.node.name, env_ids)
                        })
                        .collect(),
                    has_restricted,
                )
            }
            Project::Workspace(project) => {
                let total_envs = project.environments.edges.len();
                let mut service_env_map = build_service_env_map!(&project.environments.edges);
                let accessible_envs: Vec<_> = project
                    .environments
                    .edges
                    .into_iter()
                    .filter(|env| env.node.can_access)
                    .map(|env| NormalisedEnvironment::new(env.node.id, env.node.name))
                    .collect();
                let has_restricted = total_envs > accessible_envs.len();
                NormalisedProject::new(
                    project.id,
                    project.name,
                    accessible_envs,
                    project
                        .services
                        .edges
                        .into_iter()
                        .map(|service| {
                            let env_ids =
                                service_env_map.remove(&service.node.id).unwrap_or_default();
                            NormalisedService::new(service.node.id, service.node.name, env_ids)
                        })
                        .collect(),
                    has_restricted,
                )
            }
        }
    }
}

impl Display for NormalisedEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl Display for NormalisedService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}
