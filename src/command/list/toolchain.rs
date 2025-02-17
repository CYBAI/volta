use std::ffi::OsString;
use std::rc::Rc;

use semver::Version;

use super::{Filter, Node, Package, PackageManager, Source};
use crate::command::list::PackageManagerKind;
use volta_core::{
    inventory::Inventory, platform::PlatformSpec, project::Project, tool::PackageConfig,
};
use volta_fail::Fallible;

pub(super) enum Toolchain {
    Node(Vec<Node>),
    PackageManagers(Vec<PackageManager>),
    Packages(Vec<Package>),
    Tool {
        name: String,
        host_packages: Vec<Package>,
    },
    Active {
        runtime: Option<Node>,
        package_manager: Option<PackageManager>,
        packages: Vec<Package>,
    },
    All {
        runtimes: Vec<Node>,
        package_managers: Vec<PackageManager>,
        packages: Vec<Package>,
    },
}

/// Lightweight rule for which item to get the `Source` for.
enum Lookup {
    /// Look up the Node runtime
    Runtime,
    /// Look up the Yarn package manager
    Yarn,
}

impl Lookup {
    fn version_from_spec(self) -> impl Fn(Rc<PlatformSpec>) -> Option<Version> {
        move |spec| match self {
            Lookup::Runtime => Some(spec.node_runtime.clone()),
            Lookup::Yarn => spec.yarn.clone(),
        }
    }

    fn version_source<'p>(
        self,
        project: &'p Option<Rc<Project>>,
        user_platform: &Option<Rc<PlatformSpec>>,
        version: &Version,
    ) -> Source {
        match project {
            Some(project) => project
                .platform()
                .and_then(self.version_from_spec())
                .and_then(|project_version| match &project_version == version {
                    true => Some(Source::Project(project.package_file())),
                    false => None,
                }),
            None => user_platform
                .clone()
                .and_then(self.version_from_spec())
                .and_then(|ref default_version| match default_version == version {
                    true => Some(Source::Default),
                    false => None,
                }),
        }
        .unwrap_or(Source::None)
    }

    /// Determine the `Source` for a given kind of tool (`Lookup`).
    fn active_tool(
        self,
        project: &Option<Rc<Project>>,
        user: &Option<Rc<PlatformSpec>>,
    ) -> Option<(Source, Version)> {
        match project {
            Some(project) => project
                .platform()
                .and_then(self.version_from_spec())
                .map(|version| (Source::Project(project.package_file()), version)),
            None => user
                .clone()
                .and_then(self.version_from_spec())
                .map(|version| (Source::Default, version)),
        }
    }
}

/// Look up the `Source` for a tool with a given name.
fn tool_source(name: &str, version: &Version, project: &Option<Rc<Project>>) -> Fallible<Source> {
    match project {
        Some(project) => {
            let project_version_is_tool_version = project
                .as_ref()
                .matching_bin(&OsString::from(name), version)?
                .map_or(false, |bin| &bin.version == version);

            if project_version_is_tool_version {
                Ok(Source::Project(project.package_file()))
            } else {
                Ok(Source::Default)
            }
        }
        _ => Ok(Source::Default),
    }
}

impl Toolchain {
    pub(super) fn active(
        project: &Option<Rc<Project>>,
        user_platform: &Option<Rc<PlatformSpec>>,
        inventory: &Inventory,
    ) -> Fallible<Toolchain> {
        let runtime = Lookup::Runtime
            .active_tool(project, user_platform)
            .map(|(source, version)| Node { source, version });

        let package_manager =
            Lookup::Yarn
                .active_tool(project, user_platform)
                .map(|(source, version)| PackageManager {
                    kind: PackageManagerKind::Yarn,
                    source,
                    version,
                });

        let packages = Package::from_inventory_and_project(inventory, project);

        Ok(Toolchain::Active {
            runtime,
            package_manager,
            packages,
        })
    }

    pub(super) fn all(
        project: &Option<Rc<Project>>,
        user_platform: &Option<Rc<PlatformSpec>>,
        inventory: &Inventory,
    ) -> Fallible<Toolchain> {
        let runtimes = inventory
            .node
            .versions
            .iter()
            .map(|version| Node {
                source: Lookup::Runtime.version_source(project, user_platform, version),
                version: version.clone(),
            })
            .collect();

        let package_managers = inventory
            .yarn
            .versions
            .iter()
            .map(|version| PackageManager {
                kind: PackageManagerKind::Yarn,
                source: Lookup::Yarn.version_source(project, user_platform, version),
                version: version.clone(),
            })
            .collect();

        let packages = Package::from_inventory_and_project(inventory, project);

        Ok(Toolchain::All {
            runtimes,
            package_managers,
            packages,
        })
    }

    pub(super) fn node(
        inventory: &Inventory,
        project: &Option<Rc<Project>>,
        user_platform: &Option<Rc<PlatformSpec>>,
        filter: &Filter,
    ) -> Toolchain {
        let runtimes = inventory
            .node
            .versions
            .iter()
            .filter_map(|version| {
                let source = Lookup::Runtime.version_source(project, user_platform, version);
                if source.allowed_with(filter) {
                    let version = version.clone();
                    Some(Node { source, version })
                } else {
                    None
                }
            })
            .collect();

        Toolchain::Node(runtimes)
    }

    pub(super) fn yarn(
        inventory: &Inventory,
        project: &Option<Rc<Project>>,
        user_platform: &Option<Rc<PlatformSpec>>,
        filter: &Filter,
    ) -> Toolchain {
        let yarns = inventory
            .yarn
            .versions
            .iter()
            .filter_map(|version| {
                let source = Lookup::Yarn.version_source(project, user_platform, version);
                if source.allowed_with(filter) {
                    Some(PackageManager {
                        kind: PackageManagerKind::Yarn,
                        source,
                        version: version.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();

        Toolchain::PackageManagers(yarns)
    }

    pub(super) fn package_or_tool(
        name: &str,
        inventory: &Inventory,
        project: &Option<Rc<Project>>,
        filter: &Filter,
    ) -> Fallible<Toolchain> {
        /// An internal-only helper for tracking whether we found a given item
        /// from the `PackageCollection` as a *package* or as a *tool*.
        #[derive(PartialEq, Debug)]
        enum Kind {
            Package,
            Tool,
        }

        /// A convenient name for this tuple, since we have to name it in a few
        /// spots below.
        type Triple<'p> = (Kind, &'p PackageConfig, Source);

        let packages_and_tools = inventory
            .packages
            .iter()
            .filter_map(|config| {
                // Start with the package itself, since tools often match
                // the package name and we prioritize packages.
                if &config.name == name {
                    let source = Package::source(name, &config.version, project);
                    if source.allowed_with(filter) {
                        Some(Ok((Kind::Package, config, source)))
                    } else {
                        None
                    }

                // Then check if the passed name matches an installed package's
                // binaries. If it does, we have a tool.
                } else if config
                    .bins
                    .iter()
                    .find(|bin| bin.as_str() == name)
                    .is_some()
                {
                    tool_source(name, &config.version, project)
                        .map(|source| {
                            if source.allowed_with(filter) {
                                Some((Kind::Tool, config, source))
                            } else {
                                None
                            }
                        })
                        .transpose()

                // Otherwise, we don't have any match all.
                } else {
                    None
                }
            })
            // Then eagerly collect the first error (if there are any) and
            // return it; otherwise we have a totally valid collection.
            .collect::<Fallible<Vec<Triple>>>()?;

        let (has_packages, has_tools) =
            packages_and_tools
                .iter()
                .fold((false, false), |(packages, tools), (kind, ..)| {
                    (
                        packages || kind == &Kind::Package,
                        tools || kind == &Kind::Tool,
                    )
                });

        let toolchain = match (has_packages, has_tools) {
            // If there are neither packages nor tools, treat it as `Packages`,
            // but don't re-process the data just to construct an empty `Vec`!
            (false, false) => Toolchain::Packages(vec![]),
            // If there are any packages, we resolve this *as* `Packages`, even
            // if there are also matching tools, since we give priority to
            // listing packages between packages and tools.
            (true, _) => {
                let packages = packages_and_tools
                    .into_iter()
                    .filter_map(|(kind, config, source)| match kind {
                        Kind::Package => Some(Package::new(&config, &source)),
                        Kind::Tool => None,
                    })
                    .collect();

                Toolchain::Packages(packages)
            }
            // If there are no packages matching, but we do have tools matching,
            // we return `Tool`.
            (false, true) => {
                let host_packages = packages_and_tools
                    .into_iter()
                    .filter_map(|(kind, config, source)| match kind {
                        Kind::Tool => Some(Package::new(&config, &source)),
                        Kind::Package => None, // should be none of these!
                    })
                    .collect();

                Toolchain::Tool {
                    name: name.into(),
                    host_packages,
                }
            }
        };

        Ok(toolchain)
    }
}
