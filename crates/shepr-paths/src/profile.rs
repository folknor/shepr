use shepr_core::env::{EnvVar, SHARED_APP_DIR_NAME};

include!(concat!(env!("OUT_DIR"), "/build_profile.rs"));

/// The build profile a binary was compiled with, which decides where it keeps
/// its runtime sockets and its saved layout and history.
///
/// A release build uses the default XDG locations. Every other build (the
/// cargo dev profile) uses `shepr-dev` in place of `shepr` for the runtime
/// directory and the saved-layout directory, so a dev server and the installed
/// release server hold different sockets, locks and saved layouts without any
/// flag. Both config files and the client-owned state stay shared by every
/// profile.
/// A server of another build is still refused by the build-identity checks,
/// which is what tells the two apart once they can no longer collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildProfile {
    Release,
    Dev,
}

impl BuildProfile {
    /// The profile this crate was built with, from cargo's `PROFILE`.
    pub const fn current() -> Self {
        Self::from_cargo_profile(BUILD_PROFILE)
    }

    /// `release` (and any profile inheriting from it) is [`Release`](Self::Release);
    /// everything else is [`Dev`](Self::Dev).
    pub(crate) const fn from_cargo_profile(profile: &str) -> Self {
        // A const fn cannot compare strs with `==`.
        match profile.as_bytes() {
            b"release" => Self::Release,
            _ => Self::Dev,
        }
    }

    /// The value a pane exports as `SHEPR_BUILD_PROFILE` to name the profile of
    /// the server that owns it.
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Dev => "dev",
        }
    }

    fn from_marker(value: &str) -> Option<Self> {
        match value {
            "release" => Some(Self::Release),
            "dev" => Some(Self::Dev),
            _ => None,
        }
    }

    /// The directory name this profile uses under the XDG runtime directory
    /// and beside the shared state directory.
    pub const fn app_dir_name(self) -> &'static str {
        match self {
            Self::Release => SHARED_APP_DIR_NAME,
            Self::Dev => "shepr-dev",
        }
    }
}

/// The relationship between this process and the server named by its pane
/// markers. Unknown profile markers are refused while reading the marker, so
/// a pane cannot quietly be treated as an unrelated build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneOwner {
    NotInPane,
    SameProfile,
    OtherProfile,
}

/// Typed values read once from the two environment markers. The profile also
/// guides socket selection when a script sets the marker without `SHEPR_ENV`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PaneMarker {
    pub(crate) in_pane: bool,
    pub(crate) owner_profile: Option<BuildProfile>,
}

impl PaneMarker {
    pub(crate) fn read(problems: &mut Vec<String>) -> Self {
        let in_pane = match shepr_core::env::read_text(EnvVar::SheprEnv) {
            Ok(value) => value.as_deref() == Some(shepr_core::env::SHEPR_ENV_IN_PANE),
            Err(error) => {
                problems.push(error.to_string());
                false
            }
        };
        let owner_profile = Self::read_profile(problems);
        Self {
            in_pane,
            owner_profile,
        }
    }

    pub(crate) fn read_profile(problems: &mut Vec<String>) -> Option<BuildProfile> {
        match shepr_core::env::read_text(EnvVar::SheprBuildProfile) {
            Ok(Some(marker)) => match BuildProfile::from_marker(&marker) {
                Some(profile) => Some(profile),
                None => {
                    problems.push(format!(
                        "{} must be `release` or `dev`, got `{marker}`",
                        EnvVar::SheprBuildProfile
                    ));
                    None
                }
            },
            Ok(None) => None,
            Err(error) => {
                problems.push(error.to_string());
                None
            }
        }
    }

    pub(crate) fn owner(self, current_profile: BuildProfile) -> PaneOwner {
        if !self.in_pane {
            PaneOwner::NotInPane
        } else if self
            .owner_profile
            .is_some_and(|owner| owner != current_profile)
        {
            PaneOwner::OtherProfile
        } else {
            PaneOwner::SameProfile
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_profile_names_map_to_build_profiles() {
        assert_eq!(
            BuildProfile::from_cargo_profile("release"),
            BuildProfile::Release
        );
        assert_eq!(BuildProfile::from_cargo_profile("debug"), BuildProfile::Dev);
        assert_eq!(BuildProfile::from_cargo_profile(""), BuildProfile::Dev);
        assert_eq!(BuildProfile::Release.app_dir_name(), "shepr");
        assert_eq!(BuildProfile::Dev.app_dir_name(), "shepr-dev");
    }
}
