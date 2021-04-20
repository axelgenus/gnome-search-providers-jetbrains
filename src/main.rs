// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#![deny(warnings, missing_docs, clippy::all)]

//! Gnome search provider for Jetbrains products

use std::collections::HashMap;
use std::convert::TryInto;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use elementtree::Element;
use gio::{AppInfoExt, IconExt};
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use regex::Regex;
use std::borrow::Borrow;
use zbus::export::zvariant;
use zbus::{dbus_interface, fdo};

/// A path with an associated version.
#[derive(Debug)]
struct VersionedPath {
    path: PathBuf,
    /// The version as pair of epoch and major version.
    version: (u16, u16),
}

/// Read paths of all recent projects from the given `reader`.
fn read_recent_jetbrains_projects<R: Read>(reader: R) -> Result<Vec<PathBuf>> {
    let element = Element::from_reader(reader)?;
    let home = dirs::home_dir()
        .with_context(|| "$HOME directory required")?
        .into_os_string()
        .into_string()
        .ok()
        .with_context(|| "$HOME not a valid UTF-8 string")?;

    let projects = element
        .find_all("component")
        .find(|e| e.get_attr("name") == Some("RecentProjectsManager"))
        .and_then(|comp| {
            comp.find_all("option")
                .find(|e| e.get_attr("name") == Some("additionalInfo"))
        })
        .and_then(|opt| opt.find("map"))
        .map(|map| {
            map.find_all("entry")
                .filter_map(|entry| entry.get_attr("key"))
                .map(|key| Path::new(&key.replace("$USER_HOME$", &home)).to_path_buf())
                .collect()
        })
        .unwrap_or_default();

    Ok(projects)
}

impl VersionedPath {
    /// Extract the version number from the given path.
    ///
    /// Return `None` if the path doesn't contain any valid version.
    fn extract_version(path: PathBuf) -> Option<VersionedPath> {
        lazy_static! {
            static ref RE: Regex = Regex::new(r"(\d{1,4}).(\d{1,2})").unwrap();
        }

        let version = path
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(|filename| RE.captures(filename))
            .map(|m| (u16::from_str(&m[1]).unwrap(), u16::from_str(&m[2]).unwrap()));

        version.map(|version| VersionedPath { path, version })
    }

    /// Get the path out of this versioned path
    fn into_path(self) -> PathBuf {
        self.path
    }
}

/// A location for configuration of a Jetbrains product.
#[derive(Debug)]
struct ConfigLocation<'a> {
    /// The vendor configuration directory.
    vendor_dir: &'a str,
    /// A glob for configuration directories inside the vendor directory.
    config_glob: &'a str,
    /// The file name for recent projects
    projects_filename: &'a str,
}

impl ConfigLocation<'_> {
    /// Find the configuration directory of the latest installed product version.
    fn find_config_dir_of_latest_version(&self, config_home: &Path) -> Option<VersionedPath> {
        let vendor_dir = config_home.join(self.vendor_dir);
        globwalk::GlobWalkerBuilder::new(vendor_dir, self.config_glob)
            .build()
            .expect("Failed to build glob pattern")
            .filter_map(Result::ok)
            .map(globwalk::DirEntry::into_path)
            .filter_map(VersionedPath::extract_version)
            .max_by_key(|p| p.version)
    }

    /// Find the latest recent projects file.
    fn find_latest_recent_projects_file(&self, config_home: &Path) -> Option<PathBuf> {
        self.find_config_dir_of_latest_version(config_home)
            .map(|p| p.into_path())
            .map(|p| p.join("options").join(self.projects_filename))
            .filter(|p| p.is_file())
    }
}

/// Get the name of the Jetbrains product at the given path.
///
/// Look for a `name` file in the `.idea` sub-directory; if that file does not exist
/// or cannot be read take the file name of `path`, and ultimately return `None` if
/// the name cannot be determined.
fn get_project_name(path: &Path) -> Option<String> {
    File::open(path.join(".idea").join("name"))
        .and_then(|mut source| {
            let mut buffer = String::new();
            source.read_to_string(&mut buffer)?;
            Ok(buffer)
        })
        .ok()
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
}

/// A search provider to expose from this service.
struct ProviderDefinition<'a> {
    /// A human readable label for this provider.
    label: &'a str,
    /// The ID (that is, the filename) of the desktop file of the corresponding app.
    desktop_id: &'a str,
    /// The relative object path to expose this provider at.
    relative_obj_path: &'a str,
    /// The location of the configuration of the corresponding product.
    config: ConfigLocation<'a>,
}

impl ProviderDefinition<'_> {
    /// Gets the full object path for this provider.
    fn objpath(&self) -> String {
        format!(
            "/de/swsnr/searchprovider/jetbrains/{}",
            self.relative_obj_path
        )
    }
}

/// Known search providers.
///
/// For each definition in this array a corresponding provider file must exist in
/// `providers/`; the file must refer to the same `desktop_id` and the same object path.
/// The object path must be unique for each desktop ID, to ensure that this service always
/// launches the right application associated with the search provider.
const PROVIDERS: &[ProviderDefinition] = &[
    ProviderDefinition {
        label: "CLion (toolbox)",
        desktop_id: "jetbrains-clion.desktop",
        relative_obj_path: "toolbox/clion",
        config: ConfigLocation {
            vendor_dir: "JetBrains",
            config_glob: "CLion*",
            projects_filename: "recentProjects.xml",
        },
    },
    ProviderDefinition {
        label: "GoLand (toolbox)",
        desktop_id: "jetbrains-goland.desktop",
        relative_obj_path: "toolbox/goland",
        config: ConfigLocation {
            vendor_dir: "JetBrains",
            config_glob: "GoLand*",
            projects_filename: "recentProjects.xml",
        },
    },
    ProviderDefinition {
        label: "IDEA (toolbox)",
        desktop_id: "jetbrains-idea.desktop",
        relative_obj_path: "toolbox/idea",
        config: ConfigLocation {
            vendor_dir: "JetBrains",
            config_glob: "IntelliJIdea*",
            projects_filename: "recentProjects.xml",
        },
    },
    ProviderDefinition {
        label: "IDEA Community Edition (toolbox)",
        desktop_id: "jetbrains-idea-ce.desktop",
        relative_obj_path: "toolbox/ideace",
        config: ConfigLocation {
            vendor_dir: "JetBrains",
            config_glob: "IdeaIC*",
            projects_filename: "recentProjects.xml",
        },
    },
    ProviderDefinition {
        label: "PHPStorm (toolbox)",
        desktop_id: "jetbrains-phpstorm.desktop",
        relative_obj_path: "toolbox/phpstorm",
        config: ConfigLocation {
            vendor_dir: "JetBrains",
            config_glob: "PhpStorm*",
            projects_filename: "recentProjects.xml",
        },
    },
    ProviderDefinition {
        label: "PyCharm (toolbox)",
        desktop_id: "jetbrains-pycharm.desktop",
        relative_obj_path: "toolbox/pycharm",
        config: ConfigLocation {
            vendor_dir: "JetBrains",
            config_glob: "PyCharm*",
            projects_filename: "recentProjects.xml",
        },
    },
    ProviderDefinition {
        label: "Rider (toolbox)",
        desktop_id: "jetbrains-rider.desktop",
        relative_obj_path: "toolbox/rider",
        config: ConfigLocation {
            vendor_dir: "JetBrains",
            config_glob: "Rider*",
            projects_filename: "recentSolutions.xml",
        },
    },
    ProviderDefinition {
        label: "RubyMine (toolbox)",
        desktop_id: "jetbrains-rubymine.desktop",
        relative_obj_path: "toolbox/rubymine",
        config: ConfigLocation {
            vendor_dir: "JetBrains",
            config_glob: "RubyMine*",
            projects_filename: "recentProjects.xml",
        },
    },
    ProviderDefinition {
        label: "Android Studio (toolbox)",
        desktop_id: "jetbrains-studio.desktop",
        relative_obj_path: "toolbox/studio",
        config: ConfigLocation {
            vendor_dir: "Google",
            config_glob: "AndroidStudio*",
            projects_filename: "recentProjects.xml",
        },
    },
    ProviderDefinition {
        label: "WebStorm (toolbox)",
        desktop_id: "jetbrains-webstorm.desktop",
        relative_obj_path: "toolbox/webstorm",
        config: ConfigLocation {
            vendor_dir: "JetBrains",
            config_glob: "WebStorm*",
            projects_filename: "recentProjects.xml",
        },
    },
];

/// A recent project of a Jetbrains product.
#[derive(Debug, PartialEq)]
struct RecentProject {
    /// The human readable name.
    name: String,
    /// The project directory.
    path: PathBuf,
}

/// Find all projects from `projects` which match the given `terms`.
///
/// `projects` is an iterator over pairs of `(id, project)`.
///
/// For each `project` match `terms` against the name and the `path` and return
/// a vector with all `id`s of projects which match.
///
/// Currently this simply checks whether all terms are container in `path` or `name`
/// but this is subject to change.
fn find_matching_projects<'a, I, S, T, P>(
    projects: I,
    terms: &'a [S],
) -> impl Iterator<Item = T> + 'a
where
    I: Iterator<Item = (T, P)> + 'a,
    P: Borrow<RecentProject>,
    T: AsRef<str>,
    S: AsRef<str>,
{
    projects.filter_map(move |(id, project)| {
        Some(id).filter(move |_| {
            let path = project.borrow().path.as_os_str().to_str();
            terms.iter().all(|term| {
                project
                    .borrow()
                    .name
                    .to_lowercase()
                    .contains(&term.as_ref().to_lowercase())
            }) || path.map_or(false, |p| {
                terms
                    .iter()
                    .all(|term| p.to_lowercase().contains(&term.as_ref().to_lowercase()))
            })
        })
    })
}

/// A DBus search provider for a Jetbrains app.
struct JetbrainsSearchProvider<'a> {
    /// The app to launch for search results.
    app: gio::DesktopAppInfo,
    /// Where to look for the configuration and the list of recent projects.
    config: &'a ConfigLocation<'a>,
    /// All known recents projects.
    projects: HashMap<String, RecentProject>,
}

impl<'a> JetbrainsSearchProvider<'a> {
    /// Update recent projects.
    ///
    /// Clears the map of recent projects and reads the recent projects file again.
    ///
    /// If the file fails to read return the corresponding error and leave the map of projects empty.
    fn update_recent_projects(&mut self) -> Result<()> {
        info!(
            "Updating recent projects for {}",
            self.app.get_id().unwrap()
        );
        self.projects.clear();
        let config_home = dirs::config_dir().unwrap();
        if let Some(projects_file) = self.config.find_latest_recent_projects_file(&config_home) {
            for path in read_recent_jetbrains_projects(File::open(projects_file)?)? {
                if let Some(name) = get_project_name(&path) {
                    let id = format!(
                        "jetbrains-search-provider-{}-{}",
                        self.app.get_id().unwrap(),
                        path.display()
                    );
                    self.projects.insert(id, RecentProject { name, path });
                }
            }
        };
        info!(
            "Found {} project(s) for {}",
            self.projects.len(),
            self.app.get_id().unwrap()
        );
        Ok(())
    }
}

/// The DBus interface of the search provider.
///
/// See <https://developer.gnome.org/SearchProvider/> for information.
#[dbus_interface(name = "org.gnome.Shell.SearchProvider2")]
impl JetbrainsSearchProvider<'static> {
    /// Starts a search.
    ///
    /// This function is called when a new search is started. It gets an array of search terms as arguments,
    /// and should return an array of result IDs. gnome-shell will call GetResultMetas for (some) of these result
    /// IDs to get details about the result that can be be displayed in the result list.
    fn get_initial_result_set(&mut self, terms: Vec<String>) -> zbus::fdo::Result<Vec<String>> {
        debug!(
            "Searching for {:?} of {}",
            terms,
            self.app.get_id().unwrap()
        );
        self.update_recent_projects().map_err(|error| {
            error!(
                "Failed to update recent projects for {} at {:?}: {}",
                self.app.get_id().unwrap(),
                self.config,
                error
            );
            zbus::fdo::Error::Failed(format!(
                "Failed to update recent projects for {}: {}",
                self.app.get_id().unwrap(),
                error
            ))
        })?;

        let ids = find_matching_projects(self.projects.iter(), terms.as_slice())
            .map(String::to_owned)
            .collect();
        debug!("Found ids {:?} for {}", ids, self.app.get_id().unwrap());
        Ok(ids)
    }

    /// Refine an ongoing search.
    ///
    /// This function is called to refine the initial search results when the user types more characters in the search entry.
    /// It gets the previous search results and the current search terms as arguments, and should return an array of result IDs,
    /// just like GetInitialResulSet.
    fn get_subsearch_result_set(
        &self,
        previous_results: Vec<String>,
        terms: Vec<String>,
    ) -> Vec<String> {
        debug!(
            "Searching for {:?} in {:?} of {}",
            terms,
            previous_results,
            self.app.get_id().unwrap()
        );
        let candidates = previous_results
            .iter()
            .filter_map(|id| self.projects.get(id).map(|p| (id, p)));

        let ids = find_matching_projects(candidates, terms.as_slice())
            .map(String::to_owned)
            .collect();
        debug!("Found ids {:?} for {}", ids, self.app.get_id().unwrap());
        ids
    }

    /// Get metadata for results.
    ///
    /// This function is called to obtain detailed information for results.
    /// It gets an array of result IDs as arguments, and should return a matching array of dictionaries
    /// (ie one a{sv} for each passed-in result ID).
    ///
    /// The following pieces of information should be provided for each result:
    //
    //  - "id": the result ID
    //  - "name": the display name for the result
    //  - "icon": a serialized GIcon (see g_icon_serialize()), or alternatively,
    //  - "gicon": a textual representation of a GIcon (see g_icon_to_string()), or alternativly,
    //  - "icon-data": a tuple of type (iiibiiay) describing a pixbuf with width, height, rowstride, has-alpha, bits-per-sample, and image data
    //  - "description": an optional short description (1-2 lines)
    fn get_result_metas(&self, results: Vec<String>) -> Vec<HashMap<String, zvariant::Value>> {
        debug!("Getting meta info for {:?}", results);
        results
            .into_iter()
            .filter_map(|id| {
                self.projects.get(&id).map(|project| {
                    debug!("Compiling meta infor for {}", id);
                    let icon = IconExt::to_string(&self.app.get_icon().unwrap()).unwrap();
                    debug!("Using icon {} for id {}", icon, id);

                    let mut meta: HashMap<String, zvariant::Value> = HashMap::new();
                    meta.insert("id".to_owned(), id.into());
                    meta.insert("name".to_owned(), (&project.name).into());
                    meta.insert("gicon".to_owned(), icon.to_string().into());
                    meta.insert(
                        "description".to_owned(),
                        project.path.to_string_lossy().into_owned().into(),
                    );
                    meta
                })
            })
            .collect()
    }

    /// Activate an individual result.
    ///
    /// This function is called when the user clicks on an individual result to open it in the application.
    /// The arguments are the result ID, the current search terms and a timestamp.
    ///
    /// Launches the underlying Jetbrains app with the path to the selected project.
    fn activate_result(
        &self,
        id: String,
        terms: Vec<String>,
        timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        debug!("Activating result {} for {:?} at {}", id, terms, timestamp);
        if let Some(project) = self.projects.get(&id) {
            info!("Launching recent project {:?}", project);
            self.app
                .launch::<gio::AppLaunchContext>(&[gio::File::new_for_path(&project.path)], None)
                .map_err(|error| {
                    error!(
                        "Failed to launch app {} for path {}: {}",
                        self.app.get_id().unwrap(),
                        project.path.display(),
                        error
                    );
                    zbus::fdo::Error::SpawnFailed(format!(
                        "Failed to launch app {} for path {}: {}",
                        self.app.get_id().unwrap(),
                        project.path.display(),
                        error
                    ))
                })
        } else {
            error!("Project with ID {} not found", id);
            Err(zbus::fdo::Error::Failed(format!("Result {} not found", id)))
        }
    }

    /// Launch a search within the App.
    ///
    /// This function is called when the user clicks on the provider icon to display more search results in the application.
    /// The arguments are the current search terms and a timestamp.
    ///
    /// We cannot remotely popup the project manager dialog of the underlying Jetbrains App; there's no such command line flag.
    /// Hence we simply launch the app without any arguments to bring up the start screen if it's not yet running.
    fn launch_search(&self, terms: Vec<String>, timestamp: u32) -> zbus::fdo::Result<()> {
        debug!("Launching search for {:?} at {}", terms, timestamp);
        info!("Launching app {} directly", self.app.get_id().unwrap());
        self.app
            .launch::<gio::AppLaunchContext>(&[], None)
            .map_err(|error| {
                error!(
                    "Failed to launch app {}: {}",
                    self.app.get_id().unwrap(),
                    error
                );
                zbus::fdo::Error::SpawnFailed(format!(
                    "Failed to launch app {}: {}",
                    self.app.get_id().unwrap(),
                    error
                ))
            })
    }
}

/// The name to request on the bus.
const BUSNAME: &str = "de.swsnr.searchprovider.Jetbrains";

/// Starts the DBUS service loop.
///
/// Register all providers whose underlying app is installed.
fn start_dbus_service_loop() -> Result<()> {
    let connection =
        zbus::Connection::new_session().with_context(|| "Failed to connect to session bus")?;
    fdo::DBusProxy::new(&connection)?
        .request_name(BUSNAME, fdo::RequestNameFlags::ReplaceExisting.into())
        .with_context(|| "Failed to acquire bus name")?;

    let mut object_server = zbus::ObjectServer::new(&connection);
    for provider in PROVIDERS {
        if let Some(app) = gio::DesktopAppInfo::new(provider.desktop_id) {
            info!(
                "Registering provider for {} at {}",
                provider.desktop_id,
                provider.objpath()
            );
            let dbus_provider = JetbrainsSearchProvider {
                config: &provider.config,
                app,
                projects: HashMap::new(),
            };
            object_server.at(&provider.objpath().try_into()?, dbus_provider)?;
        }
    }

    loop {
        match object_server.try_handle_next() {
            Ok(None) => debug!("Interface message processed"),
            Ok(Some(message)) => warn!("Message not handled by interfaces: {:?}", message),
            Err(err) => error!("{}", err),
        }
    }
}

fn main() -> Result<()> {
    use clap::*;

    let app = app_from_crate!()
        .setting(AppSettings::UnifiedHelpMessage)
        .setting(AppSettings::DontCollapseArgsInUsage)
        .setting(AppSettings::DeriveDisplayOrder)
        .set_term_width(80)
        .after_help(
            "\
Set $RUST_LOG to control the log level",
        )
        .arg(
            Arg::with_name("providers")
                .long("--providers")
                .help("List all providers"),
        );
    let matches = app.get_matches();
    if matches.is_present("providers") {
        let mut labels: Vec<&'static str> = PROVIDERS.iter().map(|p| p.label).collect();
        labels.sort_unstable();
        for label in labels {
            println!("{}", label)
        }
        Ok(())
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

        info!(
            "Started jetbrains search provider version: {}",
            env!("CARGO_PKG_VERSION")
        );

        start_dbus_service_loop()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn versioned_path_extract() {
        let path = dirs::home_dir()
            .expect("Must have homedir for test")
            .join(".config")
            .join("JetBrains")
            .join("IdeaIC2021.1");
        let versioned_path = VersionedPath::extract_version(path).unwrap();
        assert_eq!(versioned_path.version, (2021, 1))
    }

    #[test]
    fn read_recent_projects() {
        let data: &[u8] = include_bytes!("tests/recentProjects.xml");
        let home = dirs::home_dir().unwrap();
        let projects = read_recent_jetbrains_projects(data).unwrap();

        assert_eq!(
            projects,
            vec![
                home.join("Code").join("gh").join("mdcat"),
                home.join("Code")
                    .join("gh")
                    .join("gnome-search-providers-jetbrains")
            ]
        )
    }

    mod search {
        use std::path::Path;

        use crate::{find_matching_projects, RecentProject};

        fn do_match<'a>(projects: &[(&'a str, RecentProject)], terms: &[&str]) -> Vec<&'a str> {
            find_matching_projects(projects.iter().map(|(s, p)| (*s, p)), terms).collect()
        }

        #[test]
        fn matches_something() {
            let projects = vec![(
                "foo",
                RecentProject {
                    name: "mdcat".to_string(),
                    path: Path::new("/home/foo/dev/mdcat").to_path_buf(),
                },
            )];
            assert_eq!(do_match(&projects, &["mdcat"]), ["foo"]);
        }

        /// Regression test for https://github.com/lunaryorn/gnome-search-providers-jetbrains/issues/7
        #[test]
        fn do_not_find_undesired_projects() {
            let projects = vec![
                (
                    "foo-1",
                    RecentProject {
                        name: "ui-pattern-library".to_string(),
                        path: Path::new("/home/foo/dev/something/ui-pattern-library").to_path_buf(),
                    },
                ),
                (
                    "foo-2",
                    RecentProject {
                        name: "dauntless-builder".to_string(),
                        path: Path::new("/home/foo/dev/dauntless-builder").to_path_buf(),
                    },
                ),
                (
                    "foo-3",
                    RecentProject {
                        name: "typo3-ssr".to_string(),
                        path: Path::new("/home/foo/dev/something/typo3-ssr").to_path_buf(),
                    },
                ),
            ];
            assert!(do_match(&projects, &["flutter_test_app"]).is_empty());
        }

        #[test]
        fn ignore_case_of_name() {
            let projects = vec![(
                "foo",
                RecentProject {
                    name: "mdCat".to_string(),
                    path: Path::new("/home/foo/dev/foo").to_path_buf(),
                },
            )];
            assert_eq!(do_match(&projects, &["Mdcat"]), ["foo"]);
        }

        #[test]
        fn ignore_case_of_path() {
            let projects = vec![(
                "foo",
                RecentProject {
                    name: "bar".to_string(),
                    path: Path::new("/home/foo/dev/mdcaT").to_path_buf(),
                },
            )];
            assert_eq!(do_match(&projects, &["Mdcat"]), ["foo"]);
        }
    }

    mod providers {
        use crate::{BUSNAME, PROVIDERS};
        use anyhow::{Context, Result};
        use ini::Ini;
        use std::collections::HashSet;
        use std::path::Path;

        struct ProviderFile {
            desktop_id: String,
            object_path: String,
            bus_name: String,
            version: String,
        }

        fn load_all_provider_files() -> Result<Vec<ProviderFile>> {
            let mut providers = Vec::new();
            let ini_files = globwalk::GlobWalkerBuilder::new(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("providers"),
                "*.ini",
            )
            .build()
            .unwrap();
            for entry in ini_files {
                let filepath = entry.unwrap().into_path();
                let ini = Ini::load_from_file(&filepath).with_context(|| {
                    format!("Failed to parse ini file at {}", filepath.display())
                })?;
                let provider = ProviderFile {
                    desktop_id: ini
                        .get_from(Some("Shell Search Provider"), "DesktopId")
                        .with_context(|| format!("DesktopId missing in {}", &filepath.display()))?
                        .to_string(),
                    object_path: ini
                        .get_from(Some("Shell Search Provider"), "ObjectPath")
                        .with_context(|| format!("ObjectPath missing in {}", &filepath.display()))?
                        .to_string(),
                    bus_name: ini
                        .get_from(Some("Shell Search Provider"), "BusName")
                        .with_context(|| format!("BusName missing in {}", &filepath.display()))?
                        .to_string(),
                    version: ini
                        .get_from(Some("Shell Search Provider"), "Version")
                        .with_context(|| format!("Version missing in {}", &filepath.display()))?
                        .to_string(),
                };
                providers.push(provider);
            }

            Ok(providers)
        }

        #[test]
        fn all_providers_have_a_correct_ini_file() {
            let provider_files = load_all_provider_files().unwrap();
            for provider in PROVIDERS {
                let provider_file = provider_files
                    .iter()
                    .find(|p| p.desktop_id == provider.desktop_id);
                assert!(
                    provider_file.is_some(),
                    "Provider INI missing for provider {} with desktop ID {}",
                    provider.label,
                    provider.desktop_id
                );

                assert_eq!(provider_file.unwrap().object_path, provider.objpath());
                assert_eq!(provider_file.unwrap().bus_name, BUSNAME);
                assert_eq!(provider_file.unwrap().version, "2");
            }
        }

        #[test]
        fn no_extra_ini_files_without_providers() {
            let provider_files = load_all_provider_files().unwrap();
            assert_eq!(PROVIDERS.len(), provider_files.len());
        }

        #[test]
        fn desktop_ids_are_unique() {
            let mut ids = HashSet::new();
            for provider in PROVIDERS {
                ids.insert(provider.desktop_id);
            }
            assert_eq!(PROVIDERS.len(), ids.len());
        }

        #[test]
        fn dbus_paths_are_unique() {
            let mut paths = HashSet::new();
            for provider in PROVIDERS {
                paths.insert(provider.objpath());
            }
            assert_eq!(PROVIDERS.len(), paths.len());
        }
    }
}
