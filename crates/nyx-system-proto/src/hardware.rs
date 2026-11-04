//! What the computer has, from /sys and /proc: graphics, the processor, a battery,
//! Bluetooth, and whether it's a virtual machine. NyxOS chooses drivers, power services
//! and firmware from it; the System Manager detects it, so it's never a client's say.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// A graphics card: its vendor and PCI address.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Gpu {
    /// `amd`, `intel`, `nvidia`, `virtio` or `other`.
    pub vendor: String,
    /// NixOS's form, `PCI:1:0:0` (bus, device and function in decimal).
    pub bus: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hardware {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub gpus: BTreeSet<Gpu>,
    /// `amd` or `intel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    /// It runs on a battery (a laptop or tablet).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub battery: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bluetooth: bool,
    /// The hypervisor's name, when it's a virtual machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_machine: Option<String>,
    /// The administrator chose NVIDIA's own driver (unfree) over nouveau.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nvidia_proprietary: bool,
}

impl Hardware {
    pub fn is_empty(&self) -> bool {
        self == &Hardware::default()
    }

    pub fn has_gpu(&self, vendor: &str) -> bool {
        self.gpus.iter().any(|g| g.vendor == vendor)
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default().trim().to_string()
}

fn vendor(id: &str) -> &'static str {
    match id {
        "0x8086" => "intel",
        "0x1002" | "0x1022" => "amd",
        "0x10de" => "nvidia",
        "0x1af4" | "0x1234" | "0x15ad" | "0x80ee" => "virtio",
        _ => "other",
    }
}

/// `0000:01:00.0` → `PCI:1:0:0`.
fn bus(address: &str) -> Option<String> {
    let (rest, function) = address.rsplit_once('.')?;
    let mut parts = rest.rsplit(':');
    let device = u32::from_str_radix(parts.next()?, 16).ok()?;
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let function = u32::from_str_radix(function, 16).ok()?;
    Some(format!("PCI:{bus}:{device}:{function}"))
}

/// Detect under `root` (`/` for this computer).
pub fn detect(root: &Path) -> Hardware {
    let mut h = Hardware::default();
    if let Ok(cards) = std::fs::read_dir(root.join("sys/class/drm")) {
        for c in cards.flatten() {
            let name = c.file_name().to_string_lossy().into_owned();
            // card0, not its connectors (card0-HDMI-A-1).
            if !name.strip_prefix("card").is_some_and(|n| !n.is_empty() && n.chars().all(|d| d.is_ascii_digit())) {
                continue;
            }
            let dev = c.path().join("device");
            let address = std::fs::read_link(&dev)
                .ok()
                .and_then(|l| l.file_name().map(|f| f.to_string_lossy().into_owned()))
                .unwrap_or_default();
            if let Some(b) = bus(&address) {
                h.gpus.insert(Gpu { vendor: vendor(&read(&dev.join("vendor"))).into(), bus: b });
            }
        }
    }
    let cpuinfo = read(&root.join("proc/cpuinfo"));
    h.cpu = cpuinfo
        .lines()
        .find_map(|l| l.strip_prefix("vendor_id"))
        .map(|v| v.trim_start_matches([' ', '\t', ':']).trim())
        .and_then(|v| match v {
            "GenuineIntel" => Some("intel".to_string()),
            "AuthenticAMD" | "HygonGenuine" => Some("amd".to_string()),
            _ => None,
        });
    if let Ok(supplies) = std::fs::read_dir(root.join("sys/class/power_supply")) {
        // A mouse's or headset's battery says scope Device.
        h.battery =
            supplies.flatten().any(|s| read(&s.path().join("type")) == "Battery" && read(&s.path().join("scope")) != "Device");
    }
    h.bluetooth = std::fs::read_dir(root.join("sys/class/bluetooth")).is_ok_and(|mut d| d.next().is_some());
    let hypervisor = cpuinfo.lines().any(|l| l.starts_with("flags") && l.split_whitespace().any(|f| f == "hypervisor"));
    if hypervisor {
        let dmi = |f: &str| read(&root.join("sys/class/dmi/id").join(f));
        let product = format!("{} {}", dmi("sys_vendor"), dmi("product_name")).to_lowercase();
        h.virtual_machine = Some(
            [
                ("qemu", "kvm"),
                ("kvm", "kvm"),
                ("virtualbox", "virtualbox"),
                ("innotek", "virtualbox"),
                ("vmware", "vmware"),
                ("microsoft", "hyper-v"),
            ]
            .iter()
            .find(|(k, _)| product.contains(k))
            .map_or("other", |(_, v)| v)
            .to_string(),
        );
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn card(root: &Path, n: u32, address: &str, vendor: &str) {
        let pci = root.join("sys/devices/pci0000:00").join(address);
        std::fs::create_dir_all(&pci).unwrap();
        std::fs::write(pci.join("vendor"), format!("{vendor}\n")).unwrap();
        let drm = root.join(format!("sys/class/drm/card{n}"));
        std::fs::create_dir_all(&drm).unwrap();
        symlink(&pci, drm.join("device")).unwrap();
        std::fs::create_dir_all(root.join(format!("sys/class/drm/card{n}-eDP-1"))).unwrap();
    }

    #[test]
    fn a_hybrid_laptop() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        card(r, 0, "0000:00:02.0", "0x8086");
        card(r, 1, "0000:01:00.0", "0x10de");
        std::fs::create_dir_all(r.join("proc")).unwrap();
        std::fs::write(r.join("proc/cpuinfo"), "processor\t: 0\nvendor_id\t: GenuineIntel\nflags\t\t: fpu vme sse\n").unwrap();
        for (name, ty, scope) in [("BAT0", "Battery", "System"), ("hidpp_battery_0", "Battery", "Device"), ("AC", "Mains", "")] {
            let p = r.join("sys/class/power_supply").join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("type"), ty).unwrap();
            if !scope.is_empty() {
                std::fs::write(p.join("scope"), scope).unwrap();
            }
        }
        std::fs::create_dir_all(r.join("sys/class/bluetooth/hci0")).unwrap();
        let h = detect(r);
        assert_eq!(
            h.gpus.iter().cloned().collect::<Vec<_>>(),
            vec![
                Gpu { vendor: "intel".into(), bus: "PCI:0:2:0".into() },
                Gpu { vendor: "nvidia".into(), bus: "PCI:1:0:0".into() }
            ]
        );
        assert_eq!(h.cpu.as_deref(), Some("intel"));
        assert!(h.battery && h.bluetooth && h.virtual_machine.is_none());
        assert!(h.has_gpu("nvidia") && !h.has_gpu("amd"));
    }

    #[test]
    fn a_virtual_machine_and_a_mouse_battery() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        card(r, 0, "0000:00:01.0", "0x1af4");
        std::fs::create_dir_all(r.join("proc")).unwrap();
        std::fs::write(r.join("proc/cpuinfo"), "vendor_id\t: AuthenticAMD\nflags\t\t: fpu hypervisor\n").unwrap();
        std::fs::create_dir_all(r.join("sys/class/dmi/id")).unwrap();
        std::fs::write(r.join("sys/class/dmi/id/sys_vendor"), "QEMU\n").unwrap();
        std::fs::write(r.join("sys/class/dmi/id/product_name"), "Standard PC (Q35 + ICH9, 2009)\n").unwrap();
        let p = r.join("sys/class/power_supply/hidpp_battery_0");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("type"), "Battery").unwrap();
        std::fs::write(p.join("scope"), "Device").unwrap();
        let h = detect(r);
        assert_eq!(h.gpus.iter().map(|g| g.vendor.as_str()).collect::<Vec<_>>(), vec!["virtio"]);
        assert_eq!(h.cpu.as_deref(), Some("amd"));
        assert_eq!(h.virtual_machine.as_deref(), Some("kvm"));
        assert!(!h.battery && !h.bluetooth);
    }

    #[test]
    fn pci_addresses_in_nixos_form() {
        assert_eq!(bus("0000:0a:1f.3").as_deref(), Some("PCI:10:31:3"));
        assert_eq!(bus("nonsense"), None);
    }

    #[test]
    fn empty_and_serialized() {
        assert!(Hardware::default().is_empty());
        let h = Hardware { cpu: Some("amd".into()), ..Default::default() };
        assert_eq!(serde_json::to_string(&h).unwrap(), r#"{"cpu":"amd"}"#);
    }
}
