
use std::collections::HashMap;

pub mod boot;
pub mod install;
pub mod package;
pub mod state;
pub mod store;
pub mod switch;

pub use state::{State, Who};
pub use switch::Held;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overview {
    pub installed: Named,
    pub other: Option<Other>,
    pub other_starts: bool,
    pub changed: bool,
    pub held: Held,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Other {
    pub id: String,
    pub name: String,
    pub version: String,
    pub size: u64,
}

pub fn installed_name(props: &HashMap<String, String>, version_prop: &str) -> Named {
    let name = match (props.get("ro.mi.os.version.code"), props.get("ro.build.nothing.version"), props.get("ro.build.version.release")) {
        (Some(code), _, _) => format!("HyperOS {code}"),
        (None, Some(nothing), _) => format!("Nothing OS {nothing}"),
        (None, None, Some(release)) => format!("Android {release}"),
        (None, None, None) => "Android".into(),
    };
    let version = props
        .get(version_prop)
        .or_else(|| props.get("ro.mi.os.version.incremental"))
        .or_else(|| props.get("ro.build.nothing.version"))
        .or_else(|| props.get("ro.build.display.id"))
        .cloned();
    Named { name, version }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn the_installed_system_is_named_as_it_names_itself() {
        let hyper = props(&[("ro.mi.os.version.code", "3"), ("ro.build.version.release", "16"), ("ro.mi.os.version.incremental", "OS3.0.308.0.WMACNXM")]);
        assert_eq!(installed_name(&hyper, "ro.mi.os.version.incremental"), Named { name: "HyperOS 3".into(), version: Some("OS3.0.308.0.WMACNXM".into()) });
        let custom = props(&[("ro.build.version.release", "16"), ("ro.build.display.id", "lineage_ishtar-userdebug 16 BP2A")]);
        assert_eq!(
            installed_name(&custom, "ro.mi.os.version.incremental"),
            Named { name: "Android 16".into(), version: Some("lineage_ishtar-userdebug 16 BP2A".into()) }
        );
    }
}
