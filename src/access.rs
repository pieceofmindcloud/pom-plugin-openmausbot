//! How phones reach OpenMausBot. The default keeps the server on loopback,
//! reached only through POM's authenticated proxy. The operator may add one
//! way in from outside (`--tunnel` or `--tailscale`) and, beside it, the
//! local network (`--lan`); OpenMausBot then requires pairing for every
//! device that arrives that way.
use serde_json::{json, Value};
use std::{fs, path::Path};

pub const DEFAULT_PORT: u16 = 8799;
const FILE: &str = "access.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Way {
    #[default]
    Local,
    Tunnel,
    Tailscale,
}

impl Way {
    pub fn code(self) -> &'static str {
        match self {
            Way::Local => "local",
            Way::Tunnel => "tunnel",
            Way::Tailscale => "tailscale",
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "local" => Some(Way::Local),
            "tunnel" => Some(Way::Tunnel),
            "tailscale" => Some(Way::Tailscale),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    pub way: Way,
    pub lan: bool,
    pub port: u16,
}

impl Default for Access {
    fn default() -> Self {
        Self {
            way: Way::Local,
            lan: false,
            port: DEFAULT_PORT,
        }
    }
}

impl Access {
    /// The saved choice, or the default when there is none or it is unreadable.
    pub fn load(base: &Path) -> Self {
        fs::read(base.join(FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| Self::from_json(&value).ok())
            .unwrap_or_default()
    }

    /// `{"way", "lan", "port"}`; a missing field keeps its default.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let mut access = Self::default();
        if !value.is_object() {
            return Err("phone access settings must be an object".into());
        }
        if let Some(way) = value.get("way") {
            access.way = way
                .as_str()
                .and_then(Way::parse)
                .ok_or("unknown way in")?;
        }
        if let Some(lan) = value.get("lan") {
            access.lan = lan.as_bool().ok_or("lan must be true or false")?;
        }
        if let Some(port) = value.get("port") {
            access.port = port
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .ok_or("port must be a number from 1024 to 65535")?;
        }
        Ok(access)
    }

    pub fn to_json(&self) -> Value {
        json!({"way": self.way.code(), "lan": self.lan, "port": self.port})
    }

    pub fn save(&self, base: &Path) -> Result<(), String> {
        let text = serde_json::to_vec_pretty(&self.to_json()).map_err(|error| error.to_string())?;
        fs::write(base.join(FILE), text).map_err(|_| "Cannot save the phone access settings".into())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.port < 1024 {
            return Err("Choose a port from 1024 to 65535".into());
        }
        Ok(())
    }

    /// Something outside this machine may reach the server: the port is
    /// fixed, so phones paired earlier find it again after a restart.
    pub fn exposed(&self) -> bool {
        self.way != Way::Local || self.lan
    }

    pub fn serve_args(&self) -> Vec<&'static str> {
        let mut args = match self.way {
            Way::Local => vec![],
            Way::Tunnel => vec!["--tunnel"],
            Way::Tailscale => vec!["--tailscale"],
        };
        if self.lan {
            args.push("--lan");
        }
        args
    }
}

/// `--tunnel` needs the account `openmausbot account login` saves here.
pub fn tunnel_account_saved(data: &Path) -> bool {
    data.join("tunnel-account.json").is_file()
}

/// `--tailscale` needs the `tailscale` command, signed in, on this machine.
pub fn tailscale_installed() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            dir.join("tailscale").is_file() || dir.join("tailscale.exe").is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_loopback_only() {
        let access = Access::default();
        assert!(!access.exposed());
        assert!(access.serve_args().is_empty());
    }

    #[test]
    fn one_way_in_plus_the_lan() {
        let access = Access {
            way: Way::Tunnel,
            lan: true,
            port: 9000,
        };
        assert!(access.exposed());
        assert_eq!(access.serve_args(), ["--tunnel", "--lan"]);
        let lan_only = Access {
            lan: true,
            ..Access::default()
        };
        assert_eq!(lan_only.serve_args(), ["--lan"]);
    }

    #[test]
    fn round_trips_and_survives_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("omb-access-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(Access::load(&dir), Access::default());
        let access = Access {
            way: Way::Tailscale,
            lan: false,
            port: 8800,
        };
        access.save(&dir).unwrap();
        assert_eq!(Access::load(&dir), access);
        fs::write(dir.join(FILE), "nope").unwrap();
        assert_eq!(Access::load(&dir), Access::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_the_screen_request() {
        let access = Access::from_json(&json!({"way": "tunnel", "lan": true})).unwrap();
        assert_eq!(access.way, Way::Tunnel);
        assert_eq!(access.port, DEFAULT_PORT);
        assert!(Access::from_json(&json!({"way": "ftp"})).is_err());
        assert!(Access::from_json(&json!({"port": 70000})).is_err());
    }

    #[test]
    fn refuses_privileged_ports() {
        let access = Access {
            port: 80,
            ..Access::default()
        };
        assert!(access.validate().is_err());
    }
}
