use super::*;

fn pin(name: &str, version: &str, recipe: u32, aliases: &[&str], installed: bool) -> ToolPin {
    ToolPin {
        name: name.into(),
        version: version.into(),
        recipe,
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
        core_version: None,
        installed,
    }
}

fn rev(r: &str, t: i64) -> Rev {
    Rev { rev: r.into(), last_modified: t }
}

/// A machine on stable with Claude Code, Codex and Cursor Agent, the Cursor IDE from
/// nixpkgs, Nyx Mail, oh-my-pi and a Firefox in its profile, GIMP from Flathub and a
/// laptop whose firmware has an update.
fn machine() -> Snapshot {
    let mut tools = BTreeMap::new();
    tools.insert("claude-code".into(), pin("Claude Code", "2.1.152", 1, &["claude"], true));
    tools.insert("codex".into(), pin("Codex CLI", "0.134.0", 1, &[], true));
    tools.insert("cursor-cli".into(), pin("Cursor Agent", "2026.05.16-0338208", 1, &["cursor-agent", "cursor"], true));
    tools.insert("crush".into(), pin("Crush", "0.70.0", 1, &[], false));
    let mut head = tools.clone();
    head.get_mut("claude-code").unwrap().version = "2.1.311".into();
    // Codex's new release needs a new recipe: only the system update brings it.
    let codex = head.get_mut("codex").unwrap();
    codex.version = "0.167.0".into();
    codex.recipe = 2;
    Snapshot {
        channel: Channel {
            url: Some("github:nyxos-project/nyxos/stable".into()),
            branch: "stable".into(),
            installed: Some(rev("434e34eaaaaa", 1_793_836_800)),
            head: Some(rev("9f00ba1bbbbb", 1_794_009_600)),
            error: None,
        },
        tools,
        head_tools: Some(head),
        tool_aliases: BTreeMap::from([("cursor-cli".to_string(), vec!["cursor-agent".to_string()])]),
        apps: vec![
            ChannelApp { id: "cursor".into(), name: "Cursor".into(), aliases: vec!["cursor".into(), "code-cursor".into()] },
            ChannelApp { id: "nyx-mail".into(), name: "Nyx Mail".into(), aliases: vec!["mail".into()] },
        ],
        profile: vec![
            ProfileElement {
                name: "oh-my-pi".into(),
                url: "github:can1357/oh-my-pi".into(),
                tool: true,
                aliases: vec!["omp".into()],
            },
            ProfileElement { name: "firefox".into(), url: "nixpkgs#firefox".into(), tool: false, aliases: vec![] },
        ],
        flatpaks: vec![Flatpak {
            id: "org.gimp.GIMP".into(),
            name: "GIMP".into(),
            version: Some("3.0.4".into()),
            update: Some("3.0.6".into()),
            aliases: vec!["gimp".into()],
        }],
        firmware: Firmware::Devices {
            devices: vec![Device {
                id: "abc123".into(),
                name: "System Firmware".into(),
                version: "1.10".into(),
                update: Some("1.12".into()),
                summary: None,
                needs_restart: true,
            }],
        },
        mise: vec![
            UserTool { name: "node".into(), version: Some("22.1.0".into()), update: Some("22.2.0".into()), aliases: vec![] },
            UserTool {
                name: "github:openai/codex".into(),
                version: Some("0.167.0".into()),
                update: None,
                aliases: vec!["codex".into()],
            },
        ],
        uv_tools: vec![UserTool {
            name: "trafilatura".into(),
            version: Some("2.3.0".into()),
            update: Some("2.3.1".into()),
            aliases: vec![],
        }],
        checked: true,
    }
}

fn plan_for(s: &Snapshot, names: &[&str]) -> Result<Plan> {
    let t = targets(s);
    let picks = resolve(&t, names)?;
    Ok(plan(s, &t, &picks, &format!("nyx update {}", names.join(" "))))
}

#[test]
fn only_installed_tools_are_targets() {
    let t = targets(&machine());
    assert!(t.iter().any(|t| t.id == "claude-code" && t.available.as_deref() == Some("2.1.311")));
    assert!(!t.iter().any(|t| t.id == "crush"), "crush isn't installed");
    assert!(t.iter().any(|t| t.id == "codex" && t.needs_channel));
    assert!(t.iter().any(|t| t.id == "firmware:system-firmware" && t.category == Category::Firmware));
}

#[test]
fn a_tool_updates_alone() {
    let p = plan_for(&machine(), &["claude"]).unwrap();
    assert_eq!(p.steps, vec![Step::System { channel: false, tools: vec!["claude-code".into()] }]);
    assert_eq!(p.items[0].to.as_deref(), Some("2.1.311"));
    assert!(p.admin && !p.may_restart);
}

#[test]
fn a_tool_built_differently_needs_the_system_update() {
    // "codex" alone also names mise's copy.
    let p = plan_for(&machine(), &["Codex CLI"]).unwrap();
    assert!(p.steps.is_empty() && p.suggest_system);
    assert!(p.notes[0].contains("system update"), "{:?}", p.notes);
}

#[test]
fn categories_and_targets_resolve() {
    let p = plan_for(&machine(), &["tools"]).unwrap();
    assert_eq!(
        p.steps,
        vec![
            Step::System { channel: false, tools: vec!["claude-code".into()] },
            Step::Profile { names: vec!["oh-my-pi".into()] },
            Step::Mise { tools: vec![] },
            Step::UvTools { tools: vec![] },
        ]
    );
    assert!(p.items.iter().any(|i| i.name == "node" && i.to.as_deref() == Some("22.2.0")));
    assert!(p.notes.iter().any(|n| n.contains("Codex CLI")));
    let p = plan_for(&machine(), &["system"]).unwrap();
    assert_eq!(p.steps, vec![Step::System { channel: true, tools: vec![] }]);
    assert!(p.items.iter().any(|i| i.name == "Claude Code" && i.how == "with the system channel"));
    let p = plan_for(&machine(), &["gimp", "omp"]).unwrap();
    assert_eq!(
        p.steps,
        vec![Step::Profile { names: vec!["oh-my-pi".into()] }, Step::Flatpak { apps: vec!["org.gimp.GIMP".into()] }]
    );
    assert!(!p.admin);
}

#[test]
fn everything_is_one_rebuild_and_asks_before_firmware() {
    let p = plan_for(&machine(), &[]).unwrap();
    let systems = p.steps.iter().filter(|s| matches!(s, Step::System { .. })).count();
    assert_eq!(systems, 1, "{:?}", p.steps);
    assert_eq!(p.steps[0], Step::System { channel: true, tools: vec![] });
    assert!(p.steps.contains(&Step::Profile { names: vec![] }));
    assert!(p.steps.contains(&Step::Flatpak { apps: vec![] }));
    assert!(p.steps.contains(&Step::Firmware { devices: vec!["abc123".into()] }));
    assert!(p.firmware_implied && p.may_restart);
    assert!(!p.suggest_system, "everything already includes the system");
}

#[test]
fn apps_bring_the_channel_when_it_has_news() {
    let p = plan_for(&machine(), &["apps"]).unwrap();
    assert!(p.steps.contains(&Step::System { channel: true, tools: vec![] }));
    assert!(p.steps.contains(&Step::Profile { names: vec!["firefox".into()] }));
    assert!(p.notes.iter().any(|n| n.contains("come with the system channel")));
    let mut s = machine();
    s.channel.head = s.channel.installed.clone();
    let p = plan_for(&s, &["mail"]).unwrap();
    assert!(p.steps.is_empty());
    assert!(p.notes[0].contains("as new as the system channel"));
}

#[test]
fn ambiguous_names_are_listed_not_guessed() {
    let e = plan_for(&machine(), &["cursor"]).unwrap_err().to_string();
    assert!(e.contains("could be any of these"), "{e}");
    assert!(e.contains("cursor-cli") && e.contains("Cursor Agent") && e.contains("Cursor ("), "{e}");
    assert!(plan_for(&machine(), &["cursor-agent"]).is_ok());
}

#[test]
fn unknown_names_suggest_close_ones() {
    let e = plan_for(&machine(), &["claud"]).unwrap_err().to_string();
    assert!(e.contains("did you mean") && e.contains("claude-code"), "{e}");
    let e = plan_for(&machine(), &["zzzzzz"]).unwrap_err().to_string();
    assert!(e.contains("nothing installed here is called zzzzzz") && !e.contains("did you mean"), "{e}");
}

#[test]
fn mise_and_uv_tools_are_the_users_to_update() {
    let p = plan_for(&machine(), &["node", "trafilatura"]).unwrap();
    assert_eq!(p.steps, vec![Step::Mise { tools: vec!["node".into()] }, Step::UvTools { tools: vec!["trafilatura".into()] }]);
    assert!(!p.admin, "no password for the user's own tools");
    // Codex from both NyxOS and mise: the name is listed, not guessed, and the double said.
    let e = plan_for(&machine(), &["codex"]).unwrap_err().to_string();
    assert!(e.contains("could be any of these") && e.contains("your mise tools"), "{e}");
    let s = machine();
    assert!(status(&s, &targets(&s)).notes.iter().any(|n| n.contains("Codex CLI is installed by both NyxOS and mise")));
    let p = plan_for(&machine(), &["github:openai/codex"]).unwrap();
    assert!(p.steps.is_empty() && p.notes[0].contains("up to date"));
}

#[test]
fn a_tool_that_isnt_installed_says_so() {
    let s = machine();
    let t = targets(&s);
    let e = resolve_installed(&s, &t, &["crush"]).unwrap_err().to_string();
    assert!(e.contains("Crush isn't installed here"), "{e}");
    assert!(resolve_installed(&s, &t, &["claude"]).is_ok());
    assert_eq!(resolve_installed(&s, &t, &[]).unwrap().len(), Category::ALL.len());
    assert!(resolve_installed(&s, &t, &["zzzzzz"]).unwrap_err().to_string().contains("nothing installed"));
}

#[test]
fn nothing_new_is_an_empty_plan() {
    let mut s = machine();
    s.channel.head = s.channel.installed.clone();
    s.head_tools = Some(s.tools.clone());
    s.flatpaks[0].update = None;
    s.profile.clear();
    s.firmware = Firmware::Devices { devices: vec![] };
    for t in s.mise.iter_mut().chain(&mut s.uv_tools) {
        t.update = None;
    }
    let p = plan_for(&s, &[]).unwrap();
    assert!(p.is_empty(), "{:?}", p.steps);
    let p = plan_for(&s, &["claude"]).unwrap();
    assert!(p.is_empty() && p.notes[0].contains("up to date"));
}

#[test]
fn status_lists_what_has_news() {
    let s = machine();
    let st = status(&s, &targets(&s));
    let names: Vec<&str> = st.available.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"NyxOS (stable)") && names.contains(&"Claude Code") && names.contains(&"GIMP"));
    assert!(st.available.iter().any(|i| i.name == "Codex CLI" && i.how == "with the system update"));
}

#[test]
fn dates_are_civil() {
    assert_eq!(date(0), "1970-01-01");
    assert_eq!(date(1_794_009_600), "2026-11-07");
    assert_eq!(date(951_782_400), "2000-02-29");
    assert_eq!(rev("434e34eaaaaa", 1_793_836_800).describe(), "434e34e (2026-11-05)");
}
