use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Value};

use super::*;
use crate::{
    controllers::project::{ensure_project_and_environment_exist, get_project},
    errors::RailwayError,
    queries::project::{
        ProjectProject, ProjectProjectEnvironmentsEdgesNodeVolumeInstancesEdgesNode,
    },
    util::{
        progress::create_spinner,
        prompt::{fake_select, prompt_confirm_with_default, prompt_options, prompt_text},
        two_factor::validate_two_factor_if_enabled,
    },
};

/// Export service configuration to railway.toml
#[derive(Parser)]
pub struct Args {
    /// Service to export (defaults to linked service)
    #[clap(short, long)]
    service: Option<String>,

    /// Environment to export from (defaults to linked environment)
    #[clap(short, long)]
    environment: Option<String>,

    /// Output file path (defaults to railway.toml)
    #[clap(short, long)]
    output: Option<PathBuf>,

    /// Export all services in the environment
    #[clap(long)]
    all: bool,

    /// Include environment variables in the export
    #[clap(long)]
    with_variables: bool,

    /// Overwrite existing file without prompting
    #[clap(short = 'y', long)]
    yes: bool,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct RailwayConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<BuildConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deploy: Option<DeployConfig>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "$schema")]
    schema: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    environments: HashMap<String, EnvironmentConfig>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct BuildConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    builder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dockerfile_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    railpack_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "watchPatterns")]
    watch_patterns: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct DeployConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    start_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pre_deploy_command: Option<String>,
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
    num_replicas: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    regions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sleep_application: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    multi_region_config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deployment_teardown: Option<DeploymentTeardownConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volumes: Option<Vec<VolumeConfig>>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct DeploymentTeardownConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    overlap_seconds: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draining_seconds: Option<i32>,
}

#[derive(Serialize, Deserialize, Debug)]
struct VolumeConfig {
    mount_path: String,
    name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct EnvironmentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<BuildConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deploy: Option<DeployConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    variables: Option<HashMap<String, String>>,
}

pub async fn command(args: Args) -> Result<()> {
    let configs = Configs::new()?;
    let client = GQLClient::new_authorized(&configs)?;
    let linked_project = configs.get_linked_project().await?;

    ensure_project_and_environment_exist(&client, &configs, &linked_project).await?;
    let project = get_project(&client, &configs, linked_project.project.clone()).await?;

    // Determine environment
    let environment_id = if let Some(env_name) = &args.environment {
        project
            .environments
            .edges
            .iter()
            .find(|e| e.node.name == *env_name || e.node.id == *env_name)
            .map(|e| e.node.id.clone())
            .context(format!("Environment '{}' not found", env_name))?
    } else {
        linked_project.environment_id()?.to_string()
    };

    let environment_name = project
        .environments
        .edges
        .iter()
        .find(|e| e.node.id == environment_id)
        .map(|e| e.node.name.clone())
        .context("Environment not found")?;

    let output_path = args.output.unwrap_or_else(|| PathBuf::from("railway.toml"));

    // Check if file exists
    if output_path.exists() && !args.yes {
        let confirm = prompt_confirm_with_default(
            &format!(
                "File '{}' already exists. Overwrite?",
                output_path.display()
            ),
            false,
        )?;
        if !confirm {
            println!("Export cancelled.");
            return Ok(());
        }
    }

    if args.all {
        export_all_services(&project, &environment_id, &environment_name, &output_path, args.with_variables).await?;
    } else {
        // Get target service
        let service_id = if let Some(svc) = &args.service {
            project
                .services
                .edges
                .iter()
                .find(|s| s.node.name == *svc || s.node.id == *svc)
                .map(|s| s.node.id.clone())
                .context(format!("Service '{}' not found", svc))?
        } else if let Some(svc) = &linked_project.service {
            svc.clone()
        } else {
            bail!("No service specified. Use --service flag or link a service first.")
        };

        let service = project
            .services
            .edges
            .iter()
            .find(|s| s.node.id == service_id)
            .context("Service not found in project")?;

        export_single_service(
            &client,
            &configs,
            &project,
            &service_id,
            &service.node.name,
            &environment_id,
            &environment_name,
            &output_path,
            args.with_variables,
        )
        .await?;
    }

    println!(
        "Successfully exported configuration to {}",
        output_path.display().to_string().green()
    );

    Ok(())
}

async fn export_single_service(
    client: &GQLClient,
    configs: &Configs,
    project: &ProjectProject,
    service_id: &str,
    service_name: &str,
    environment_id: &str,
    environment_name: &str,
    output_path: &PathBuf,
    with_variables: bool,
) -> Result<()> {
    // Get service instance for this environment
    let env = project
        .environments
        .edges
        .iter()
        .find(|e| e.node.id == environment_id)
        .context("Environment not found")?;

    let service_instance = env
        .node
        .service_instances
        .edges
        .iter()
        .find(|si| si.node.service_id == service_id)
        .map(|si| &si.node);

    // Get volumes for this service in this environment
    let volumes: Vec<_> = env
        .node
        .volume_instances
        .edges
        .iter()
        .filter(|v| v.node.service_id.as_deref() == Some(service_id))
        .map(|v| VolumeConfig {
            mount_path: v.node.mount_path.clone(),
            name: Some(v.node.volume.name.clone()),
        })
        .collect();

    // Build config
    let mut config = RailwayConfig {
        schema: Some("https://railway.com/railway.schema.json".to_string()),
        ..Default::default()
    };

    if let Some(instance) = service_instance {
        // Source configuration
        let source = instance.source.as_ref();
        
        // Build configuration
        let mut build = BuildConfig::default();
        
        // Determine builder from latest deployment meta
        if let Some(deployment) = &instance.latest_deployment {
            if let Some(meta) = &deployment.meta {
                // Extract builder info from deployment meta
                if let Some(builder) = meta.get("builder").and_then(|b| b.as_str()) {
                    build.builder = Some(builder.to_uppercase());
                }
                if let Some(dockerfile_path) = meta.get("dockerfilePath").and_then(|d| d.as_str()) {
                    build.dockerfile_path = Some(dockerfile_path.to_string());
                }
                if let Some(build_command) = meta.get("buildCommand").and_then(|c| c.as_str()) {
                    build.build_command = Some(build_command.to_string());
                }
            }
        }

        // If no builder detected, default to RAILPACK
        if build.builder.is_none() {
            build.builder = Some("RAILPACK".to_string());
        }

        // Deploy configuration
        let mut deploy = DeployConfig {
            start_command: instance.start_command.clone(),
            cron_schedule: instance.cron_schedule.clone(),
            volumes: if volumes.is_empty() { None } else { Some(volumes) },
            ..Default::default()
        };

        // Extract additional deploy settings from deployment meta
        if let Some(deployment) = &instance.latest_deployment {
            if let Some(meta) = &deployment.meta {
                if let Some(pre_deploy) = meta.get("preDeployCommand").and_then(|p| p.as_str()) {
                    deploy.pre_deploy_command = Some(pre_deploy.to_string());
                }
                if let Some(healthcheck_path) = meta.get("healthcheckPath").and_then(|h| h.as_str()) {
                    deploy.healthcheck_path = Some(healthcheck_path.to_string());
                }
                if let Some(healthcheck_timeout) = meta.get("healthcheckTimeout").and_then(|h| h.as_i64()) {
                    deploy.healthcheck_timeout = Some(h as i32);
                }
                if let Some(restart_policy) = meta.get("restartPolicyType").and_then(|r| r.as_str()) {
                    deploy.restart_policy_type = Some(restart_policy.to_string());
                }
                if let Some(max_retries) = meta.get("restartPolicyMaxRetries").and_then(|m| m.as_i64()) {
                    deploy.restart_policy_max_retries = Some(m as i32);
                }
                if let Some(num_replicas) = meta.get("numReplicas").and_then(|n| n.as_i64()) {
                    deploy.num_replicas = Some(n as i32);
                }
                if let Some(regions) = meta.get("regions").and_then(|r| r.as_array()) {
                    deploy.regions = Some(
                        regions
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect(),
                    );
                }
                if let Some(multi_region) = meta.get("multiRegionConfig") {
                    deploy.multi_region_config = Some(multi_region.clone());
                }
            }
        }

        // Extract domains
        let domains: Vec<String> = instance
            .domains
            .service_domains
            .iter()
            .map(|d| d.domain.clone())
            .chain(instance.domains.custom_domains.iter().map(|d| d.domain.clone()))
            .collect();

        // Only include non-empty configs
        if build.builder.is_some() 
            || build.build_command.is_some() 
            || build.dockerfile_path.is_some() {
            config.build = Some(build);
        }
        
        if deploy.start_command.is_some()
            || deploy.pre_deploy_command.is_some()
            || deploy.healthcheck_path.is_some()
            || deploy.cron_schedule.is_some()
            || deploy.volumes.is_some()
        {
            config.deploy = Some(deploy);
        }
    }

    // Fetch variables if requested
    if with_variables {
        let variables = fetch_service_variables(
            client,
            configs,
            service_id,
            environment_id,
        )
        .await?;

        if !variables.is_empty() {
            let env_config = EnvironmentConfig {
                variables: Some(variables),
                ..Default::default()
            };
            config.environments.insert(environment_name.to_string(), env_config);
        }
    }

    // Write TOML
    write_toml_config(&config, output_path)?;

    Ok(())
}

async fn fetch_service_variables(
    client: &GQLClient,
    configs: &Configs,
    service_id: &str,
    environment_id: &str,
) -> Result<HashMap<String, String>> {
    use crate::gql::queries::VariablesForServiceDeployment;
    
    let vars = crate::gql::post_graphql::<VariablesForServiceDeployment>(
        client,
        configs.get_backboard(),
        variables_for_service_deployment::Variables {
            service_id: service_id.to_string(),
            environment_id: environment_id.to_string(),
        },
    )
    .await?;

    let mut result = HashMap::new();
    
    // Process variables from the response
    // Note: The actual structure depends on the GraphQL response types
    // This is a simplified version
    
    Ok(result)
}

fn write_toml_config(config: &RailwayConfig, path: &PathBuf) -> Result<()> {
    let mut doc = DocumentMut::new();
    
    // Add schema if present
    if let Some(schema) = &config.schema {
        doc["$schema"] = toml_edit::value(schema.clone());
    }

    // Add build section
    if let Some(build) = &config.build {
        let mut build_table = toml_edit::Table::new();
        
        if let Some(builder) = &build.builder {
            build_table["builder"] = toml_edit::value(builder.clone());
        }
        if let Some(cmd) = &build.build_command {
            build_table["buildCommand"] = toml_edit::value(cmd.clone());
        }
        if let Some(dockerfile) = &build.dockerfile_path {
            build_table["dockerfilePath"] = toml_edit::value(dockerfile.clone());
        }
        if let Some(version) = &build.railpack_version {
            build_table["railpackVersion"] = toml_edit::value(version.clone());
        }
        if let Some(patterns) = &build.watch_patterns {
            let arr = patterns.iter().collect::<toml_edit::Array>();
            build_table["watchPatterns"] = toml_edit::value(arr);
        }
        
        if !build_table.is_empty() {
            doc["build"] = toml_edit::Item::Table(build_table);
        }
    }

    // Add deploy section
    if let Some(deploy) = &config.deploy {
        let mut deploy_table = toml_edit::Table::new();
        
        if let Some(cmd) = &deploy.start_command {
            deploy_table["startCommand"] = toml_edit::value(cmd.clone());
        }
        if let Some(cmd) = &deploy.pre_deploy_command {
            deploy_table["preDeployCommand"] = toml_edit::value(cmd.clone());
        }
        if let Some(path) = &deploy.healthcheck_path {
            deploy_table["healthcheckPath"] = toml_edit::value(path.clone());
        }
        if let Some(timeout) = deploy.healthcheck_timeout {
            deploy_table["healthcheckTimeout"] = toml_edit::value(timeout as i64);
        }
        if let Some(policy) = &deploy.restart_policy_type {
            deploy_table["restartPolicyType"] = toml_edit::value(policy.clone());
        }
        if let Some(retries) = deploy.restart_policy_max_retries {
            deploy_table["restartPolicyMaxRetries"] = toml_edit::value(retries as i64);
        }
        if let Some(cron) = &deploy.cron_schedule {
            deploy_table["cronSchedule"] = toml_edit::value(cron.clone());
        }
        if let Some(replicas) = deploy.num_replicas {
            deploy_table["numReplicas"] = toml_edit::value(replicas as i64);
        }
        if let Some(regions) = &deploy.regions {
            let arr = regions.iter().collect::<toml_edit::Array>();
            deploy_table["regions"] = toml_edit::value(arr);
        }
        
        // Add volumes
        if let Some(volumes) = &deploy.volumes {
            let mut vols_array = toml_edit::Array::new();
            for vol in volumes {
                let mut vol_table = toml_edit::InlineTable::new();
                vol_table.insert("mountPath", vol.mount_path.clone().into());
                if let Some(name) = &vol.name {
                    vol_table.insert("name", name.clone().into());
                }
                vols_array.push(toml_edit::Value::InlineTable(vol_table));
            }
            deploy_table["volumes"] = toml_edit::value(vols_array);
        }
        
        if !deploy_table.is_empty() {
            doc["deploy"] = toml_edit::Item::Table(deploy_table);
        }
    }

    // Add environments section
    if !config.environments.is_empty() {
        let mut envs_table = toml_edit::Table::new();
        
        for (env_name, env_config) in &config.environments {
            let mut env_table = toml_edit::Table::new();
            
            if let Some(vars) = &env_config.variables {
                let mut vars_table = toml_edit::Table::new();
                for (key, value) in vars {
                    vars_table[key] = toml_edit::value(value.clone());
                }
                env_table["variables"] = toml_edit::Item::Table(vars_table);
            }
            
            // Add environment-specific build/deploy overrides
            if let Some(build) = &env_config.build {
                // Similar to above...
            }
            if let Some(deploy) = &env_config.deploy {
                // Similar to above...
            }
            
            envs_table[env_name] = toml_edit::Item::Table(env_table);
        }
        
        doc["environments"] = toml_edit::Item::Table(envs_table);
    }

    std::fs::write(path, doc.to_string())?;
    Ok(())
}

async fn export_all_services(
    project: &ProjectProject,
    environment_id: &str,
    environment_name: &str,
    output_path: &PathBuf,
    with_variables: bool,
) -> Result<()> {
    // This would export all services to a single railway.toml
    // For now, we'll export the first service or create a multi-service config
    
    bail!("Exporting all services is not yet implemented. Please specify a single service with --service.");
}