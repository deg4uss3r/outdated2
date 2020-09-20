use std::collections::HashSet;
use std::collections::HashMap;
use std::path::Path;
use std::io::{stdout, Write};
use std::fmt;

use cargo::util::config::Config;
use cargo::core::Workspace;
use cargo::util::important_paths::find_root_manifest_for_wd;
use cargo::util::toml::read_manifest;
use cargo::core::source::SourceId;
use cargo::core::manifest::Manifest;
use cargo::core::dependency::Dependency;
use cargo::core::WorkspaceConfig;
use cargo::core::package::Package;

use curl::easy::Easy;

use rayon::prelude::*;
use semver::{Version, VersionReq};
use serde::{Serialize, Deserialize};
use serde_json::{Result, Value};
#[derive(Debug, Deserialize)]
pub struct CrateVersions {
    versions: Vec<CratesResp>
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
    audit_actions: Option<Vec<AuditActions>>
}

#[derive(Debug, Deserialize)]
pub struct User {
    id: u64,
    login: String,
    name: Option<String>,
    avatar: Option<String>,
    url: Option<String>
}

#[derive(Debug, Deserialize)]
pub struct AuditActions {
    action: Option<String>,
    user: User,
    time: String
}

#[derive(Debug)]
pub struct crates_io_resp {
    crate_name: String,
    version: Version,
    last_updated: String,
}

impl Default for crates_io_resp {
    fn default() -> crates_io_resp {
        crates_io_resp{
            crate_name: String::new(), 
            version: Version::new(0,0,0),
            last_updated: String::new(),
        }
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
pub struct Dep {
    name: String,
    version_req: VersionReq,
    source_id: SourceId
}
#[derive(Debug, PartialEq, Serialize)]
struct outdated {
    crate_name: String,
    dependency_name: String,
    version_in_toml: String,
    latest_version: String,
}

impl fmt::Display for outdated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}- {}: {} -> {}", self.crate_name, self.dependency_name, self.version_in_toml, self.latest_version)
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
            let d = Dep{name: dep.name_in_toml().to_string(), version_req:dep.version_req().clone(), source_id: dep.source_id()};
            this_deps.insert(d.to_owned());
        }

        deps.insert(p.name().to_string(), this_deps);        
    });

    deps
}

fn is_up_to_date(ver_req: &VersionReq, latest: &Version) -> bool {
    ver_req.matches(&latest)
}

fn get_latest_from_repo(crate_name: String) -> crates_io_resp {
    let build_url = format!("https://crates.io/api/v1/crates/{}/versions", crate_name);
    let mut data = Vec::new();
    let mut handle = Easy::new();
   
    handle.url(&build_url).unwrap();
    handle.useragent("Cargo Outdated Bot").unwrap();
    
    {
        let mut transfer = handle.transfer();
        transfer.write_function(|new_data| {
            data.extend_from_slice(new_data);
            Ok(new_data.len())
        }).unwrap();
        transfer.perform().unwrap();
    }

    let resp_string = String::from_utf8(data).unwrap();
    let versions: CrateVersions = serde_json::from_str(&resp_string).unwrap();

    let mut latest = crates_io_resp::default();

    for version in versions.versions.iter().rev() {
        if version.yanked {
            continue;
        } else {
            latest = crates_io_resp{crate_name: version.crate_name.clone(), version: Version::parse(&version.num).unwrap(), last_updated: version.updated_at.split('.').collect::<Vec<&str>>()[0].to_string()};
        } //I think this is outputting the oldest rn 

    }
    
    latest
}


fn main() {
    let mut config = match Config::default() {
        Ok(cfg) => cfg,
        Err(e) => {
            let mut shell = cargo::core::Shell::new();
            cargo::exit_with_error(e.into(), &mut shell)
        }
    };

    let cargo_home_path = match std::env::var_os("CARGO_HOME") {
        Some(path) => Some(std::path::PathBuf::from(path)),
        None => None
    };

    config.configure(
        0,
        false,
        None,
        false,
        false,
        false,
        &cargo_home_path,
        &[],
        &[],
    ).unwrap();

    let manifest_path =  find_root_manifest_for_wd(config.cwd()).unwrap();
    let curr_workspace = Workspace::new(&manifest_path, &config).unwrap();
    let source = SourceId::for_path(curr_workspace.root()).unwrap();
    let manifest_cargo = Path::join(curr_workspace.root(), "Cargo.toml");
    let t = read_manifest(&manifest_cargo, source, curr_workspace.config());
    let maybe = t.unwrap().0;

   let deps: HashMap<String, HashSet<Dep>> = match maybe {
        cargo::core::EitherManifest::Real(real_manifest) => {
            let mut dep_hash: HashMap<String, HashSet<Dep>> = HashMap::new();

            let deps: HashSet<Dep> = real_manifest.dependencies().into_iter().map(|f| Dep{name:f.name_in_toml().to_string(), version_req:f.version_req().clone(), source_id:f.source_id()}).collect();
            dep_hash.insert(real_manifest.name().to_string(), deps.to_owned());

            dep_hash
        }
        cargo::core::EitherManifest::Virtual(virtual_manifest) => {
            check_for_workspace_members(curr_workspace)    
        }
    };

    // I now have a hashmap of a dependency and a crate name as a string (with version and path)
    // I might want the string as the key, and a HashSet of the Dependencies.. hmm 
    // from here I can get the url or path of the dependency 
    // next I want to iterate over the deps, 
    //println!("{:#?}", deps);

    //TODO figure out way to only return a string when needed
    let x: Vec<outdated> = deps.par_iter().map(|(crate_name, crate_deps)| {
        let mut y: Vec<Option<outdated>> = (*crate_deps).par_iter().map(|dep| {
            if !dep.source_id.is_path() {
                let dep_latest = get_latest_from_repo(dep.name.clone());
                if !is_up_to_date(&dep.version_req, &dep_latest.version) {
                    Some(outdated{
                        crate_name: crate_name.to_string(),
                        dependency_name: dep.name.to_string(),
                        version_in_toml: dep.version_req.to_string(),
                        latest_version: dep_latest.version.to_string(),
                    })
                } else {None}
            } else {None}
        }).collect();
        y.dedup();
        y
    })
    .flatten()
    .filter(|outdated| outdated.is_some())
    .map(|o| o.unwrap())
    .collect();

    println!("{:#?}", x);
}
