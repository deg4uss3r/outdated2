use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use cargo::core::{Workspace, source::SourceId};
use cargo::util::{config::Config, important_paths::find_root_manifest_for_wd, toml::read_manifest, OptVersionReq, VersionExt};

use anyhow::{Context, Result};
use curl::easy::Easy;
use rayon::prelude::*;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
#[derive(Debug, Deserialize)]
pub struct CrateVersions {
    versions: Vec<CratesResp>,
}

#[derive(Debug, Deserialize)]
pub struct CratesResp {
    id: u64,
    #[serde(rename = "crate")]
    crate_name: String,
    num: String,
    dl_path: String,
    readme_path: Option<String>,
    updated_at: String,
    created_at: String,
    downloads: u64,
    features: HashMap<String, Vec<String>>,
    yanked: bool,
    license: Option<String>,
    links: HashMap<String, String>,
    crate_size: Option<u64>,
    published_by: Option<User>,
    audit_actions: Option<Vec<AuditActions>>,
}

#[derive(Debug, Deserialize)]
pub struct User {
    id: u64,
    login: String,
    name: Option<String>,
    avatar: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuditActions {
    action: Option<String>,
    user: User,
    time: String,
}

#[derive(Debug)]
pub struct CratesIoResp {
    crate_name: String,
    version: Version,
    last_updated: String,
}

impl Default for CratesIoResp {
    fn default() -> CratesIoResp {
        CratesIoResp {
            crate_name: String::new(),
            version: Version::new(0, 0, 0),
            last_updated: String::new(),
        }
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
pub struct Dep {
    name: String,
    version_req: VersionReq,
    source_id: SourceId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Hash, Eq)]
struct OutdatedDependency {
    dependency_name: String,
    version_in_toml: String,
    latest_version: String,
}

unsafe impl Send for OutdatedDependency {}
unsafe impl Sync for OutdatedDependency {}

struct CrateOutdated {
    outdated: HashMap<String, Vec<OutdatedDependency>>,
}

impl CrateOutdated {
    fn new() -> CrateOutdated {
        CrateOutdated {
            outdated: HashMap::new(),
        }
    }
}

impl fmt::Display for CrateOutdated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut output_string = String::new();

        for (crate_name, outdated_dep) in self.outdated.iter() {
            output_string += &format!("{}\n", crate_name);
            for (dep_num, out_dep) in outdated_dep.iter().enumerate() {
                if dep_num == 0 && outdated_dep.len() > 1 {
                    output_string += &format!(
                        "\t├── {}: {} -> {}\n",
                        out_dep.dependency_name, out_dep.version_in_toml, out_dep.latest_version
                    );
                } else if dep_num > 0 && dep_num != outdated_dep.len() - 1 {
                    output_string += &format!(
                        "\t├── {}: {} -> {}\n",
                        out_dep.dependency_name, out_dep.version_in_toml, out_dep.latest_version
                    );
                } else {
                    output_string += &format!(
                        "\t└── {}: {} -> {}\n",
                        out_dep.dependency_name, out_dep.version_in_toml, out_dep.latest_version
                    );
                }
            }
        }
        write!(f, "{}", output_string)
    }
}

// get the entire dependency graph
// if the url of the dep is a file, recurse down add to dep hashset
// if the url of the dep is an ssh or non-crates.io repo do...something?
// request the updated from https://crates.io/api/v1/crates/{crate_name}/versions
// semver the results and check if they are on the same channel (stable, alpha, beta etc)
// show updates for the channel only

fn check_for_workspace_members(ws: cargo::core::Workspace) -> HashMap<String, HashSet<Dep>> {
    let mut deps: HashMap<String, HashSet<Dep>> = HashMap::new();

    ws.members().for_each(|p| {
        let mut this_deps: HashSet<Dep> = HashSet::new();

        for dep in p.dependencies().iter() {
            let d = Dep {
                name: dep.name_in_toml().to_string(),
                version_req: match dep.version_req().clone() {
                    OptVersionReq::Any => VersionReq::parse("*").expect("Failed Parsing * or any version required"),
                    OptVersionReq::Locked(_, vr) => vr,
                    OptVersionReq::Req(vr) => vr,
                },
                source_id: dep.source_id(),
            };
            this_deps.insert(d.to_owned());
        }

        deps.insert(p.name().to_string(), this_deps);
    });
    deps
}

fn is_up_to_date(ver_req: &VersionReq, latest: &Version) -> bool {
    ver_req.matches(&latest)
}

fn get_latest_from_repo(crate_name: String) -> Result<CratesIoResp> {
    let build_url = format!("https://crates.io/api/v1/crates/{}/versions", crate_name);
    let mut data = Vec::new();
    let mut handle = Easy::new();

    handle.url(&build_url).context("Error building URL")?;
    handle
        .useragent("Cargo Outdated Bot")
        .context("Error adding user-agent to curl")?;

    {
        let mut transfer = handle.transfer();
        transfer
            .write_function(|new_data| {
                data.extend_from_slice(new_data);
                Ok(new_data.len())
            })
            .unwrap();
        transfer.perform().context("Error reaching network")?;
    }

    let resp_string = String::from_utf8(data).context("Error parsing response into string")?;
    let versions: CrateVersions =
        serde_json::from_str(&resp_string).context("Error deserializing")?;
    let mut latest = CratesIoResp::default();

    for version in versions.versions.iter().rev() {
        //get the latest version for this crate, unless the version was yanked, skip that version
        if version.yanked {
            continue;
        } else {
            if Version::parse(&version.num).context("Error parsing version")?.is_prerelease() {
                
            } 
            latest = CratesIoResp {
                crate_name: version.crate_name.clone(),
                version: Version::parse(&version.num).context("Error parsing version")?,
                last_updated: version.updated_at.split('.').collect::<Vec<&str>>()[0].to_string(),
            };
        }
    }
    Ok(latest)
}

fn create_cargo_manifest() -> Result<HashMap<String, HashSet<Dep>>> {
    let mut config = match Config::default() {
        Ok(cfg) => cfg,
        Err(e) => {
            let mut shell = cargo::core::Shell::new();
            cargo::exit_with_error(e.into(), &mut shell)
        }
    };

    let cargo_home_path = match std::env::var_os("CARGO_HOME") {
        Some(path) => Some(std::path::PathBuf::from(path)),
        None => None,
    };

    config
        .configure(
            0,
            false,
            None,
            false,
            false,
            false,
            &cargo_home_path,
            &[],
            &[],
        )
        .context("Error creating Cargo config")?;

    let manifest_path =
        find_root_manifest_for_wd(config.cwd()).context("Error getting manifest for project")?;
    let curr_workspace =
        Workspace::new(&manifest_path, &config).context("Error creating new workspace")?;
    let source = SourceId::for_path(curr_workspace.root()).context("Error creating source")?;
    let manifest_cargo = Path::join(curr_workspace.root(), "Cargo.toml");
    let t = read_manifest(&manifest_cargo, source, curr_workspace.config());
    let maybe = t?.0;

    Ok(match maybe {
        cargo::core::EitherManifest::Real(real_manifest) => {
            let mut dep_hash: HashMap<String, HashSet<Dep>> = HashMap::new();

            let deps: HashSet<Dep> = real_manifest
                .dependencies()
                .into_iter()
                .map(|f| Dep {
                    name: f.name_in_toml().to_string(),
                    version_req:  match f.version_req().clone() {
                        OptVersionReq::Any => VersionReq::parse("*").expect("Failed Parsing * or any version required"),
                        OptVersionReq::Locked(_, vr) => vr,
                        OptVersionReq::Req(vr) => vr,
                    },
                    source_id: f.source_id(),
                })
                .collect();
            dep_hash.insert(real_manifest.name().to_string(), deps.to_owned());

            dep_hash
        }
        cargo::core::EitherManifest::Virtual(_virtual_manifest) => {
            check_for_workspace_members(curr_workspace)
        }
    })
}

fn main() -> Result<()> {
    let deps = create_cargo_manifest()?;

    //let outdated = Arc::new(Mutex::new(CrateOutdated::new()));
    let mut outdated = CrateOutdated::new();
    let x: Vec<(String, OutdatedDependency)> = deps
        .par_iter()
        .map(|(crate_name, crate_deps)| {
            let mut y: Vec<Option<(String, OutdatedDependency)>> = (*crate_deps)
                .par_iter()
                .map(|dep| {
                    if !dep.source_id.is_path() {
                        let dep_latest = get_latest_from_repo(dep.name.clone()).ok()?;
                        if !is_up_to_date(&dep.version_req, &dep_latest.version) {
                            let this_dep = OutdatedDependency {
                                dependency_name: dep.name.to_string(),
                                version_in_toml: dep.version_req.to_string(),
                                latest_version: dep_latest.version.to_string(),
                            };
                            Some((crate_name.to_string(), this_dep))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect();
            y.dedup();
            y
        })
        .flatten()
        .filter(|outdated| outdated.is_some())
        .map(|o| o.unwrap())
        .collect();

    for (crate_name, out_dep) in x.iter() {
    
    //x.par_iter().for_each(|(crate_name, out_dep)| {
    //    let mut outdated_map = outdated
    //    .lock()
    //    .unwrap();
        //let crate_map = outdated_map
        let crate_map = outdated
            .outdated
            .entry(crate_name.into())
            .or_insert(Vec::new());
        crate_map.push(out_dep.clone());
    }//);

    //if outdated.lock().unwrap().outdated.is_empty() {
    if outdated.outdated.is_empty() {
        println!("All dependencies are up-to-date!");
    } else {
        //println!("{}", outdated.lock().unwrap());
        println!("{}", outdated);
    }

    Ok(())
}
