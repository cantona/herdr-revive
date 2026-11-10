//! Pure conversions between the TUI's binary split tree and the app's
//! row/column tree, and the names and argv a copy carries.

use crate::layout::{Direction, Node};
use crate::platform;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `[3] work` -> `work`: the TUI prefixes an unnamed workspace's label with
/// its number.
pub fn space_name(label: &str) -> String {
    if let Some(rest) = label.strip_prefix('[')
        && let Some((number, name)) = rest.split_once("] ")
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
    {
        return name.to_string();
    }
    label.to_string()
}

/// The TUI reports a tab as `[n] name`; `None` when nothing is left but its
/// number, which is what it shows for a tab nobody named.
pub fn tab_name(label: &str) -> Option<String> {
    let name = space_name(label);
    if name.is_empty() || name.bytes().all(|b| b.is_ascii_digit()) {
        None
    } else {
        Some(name)
    }
}

/// Where a pane starts and what it runs; `argv: None` is a shell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub cwd: String,
    pub argv: Option<Vec<String>>,
}

/// `cwd` as a person names the folder, without Linux's removed mark
/// (`platform::existing_folder`).
pub fn shown_folder(cwd: &str) -> &str {
    cwd.strip_suffix(platform::DELETED).unwrap_or(cwd)
}

/// A login shell in `$0`, or in the home folder when `$0` is gone.
pub const CD_SHELL: &str = "cd -- \"$0\" || { echo \"herdr-revive: $0 is gone\" >&2; cd || cd /; }; exec \"${SHELL:-sh}\" -l";

/// `pane.split` takes no folder and spawns its argv as it is, with
/// herdr-core's PATH and nothing left once the program ends: the pane
/// changes into its own folder and runs the program from a login shell that
/// stays (`platform::LOGIN_PANE`). The cwd and argv travel as positional
/// parameters, never inside the script, so no character in them needs
/// quoting.
pub fn wrapped(launch: &Launch) -> Vec<String> {
    match &launch.argv {
        Some(command) => platform::login_pane(&launch.cwd, command),
        None => vec![
            "sh".into(),
            "-c".into(),
            CD_SHELL.into(),
            launch.cwd.clone(),
        ],
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AppNode {
    Split {
        direction: Direction,
        children: Vec<(AppNode, f64)>,
    },
    Terminal {
        address: String,
    },
}

/// One node of the app's layout document with its weight (1 when absent).
/// `None` for a mod block (counted in `mods`) and for a row or column with
/// no terminal left in it; a row or column with one child is that child.
pub fn parse_app_node(
    value: &Value,
    depth: usize,
    mods: &mut usize,
) -> Result<Option<(AppNode, f64)>> {
    ensure!(depth <= 64, "app layout tree exceeds depth limit");
    let weight = match value.pointer("/size/weight") {
        None | Some(Value::Null) => 1.0,
        Some(weight) => weight
            .as_f64()
            .context("app layout weight is not a number")?,
    };
    ensure!(
        weight.is_finite() && weight > 0.0,
        "app layout weight must be a positive number"
    );
    let node = match value["type"].as_str() {
        Some("block") => match value.pointer("/content/terminal") {
            Some(Value::String(address)) => AppNode::Terminal {
                address: address.clone(),
            },
            _ => {
                *mods += 1;
                return Ok(None);
            }
        },
        Some(kind @ ("row" | "column")) => {
            let direction = if kind == "row" {
                Direction::Right
            } else {
                Direction::Down
            };
            let mut children = Vec::new();
            for child in value["children"]
                .as_array()
                .context("app layout row or column without children")?
            {
                if let Some(child) = parse_app_node(child, depth + 1, mods)? {
                    children.push(child);
                }
            }
            if children.len() > 1 {
                AppNode::Split {
                    direction,
                    children,
                }
            } else if let Some((only, _)) = children.pop() {
                only
            } else {
                return Ok(None);
            }
        }
        Some(other) => bail!("unsupported app layout node type {other}"),
        None => bail!("app layout node without a type"),
    };
    Ok(Some((node, weight)))
}

/// Left fold: `[a, b, c]` becomes `((a | b) | c)`, and each split's ratio is
/// the share of the weights it spans that its first part holds.
pub fn to_binary(node: &AppNode, leaf: &mut impl FnMut(&str) -> Result<String>) -> Result<Node> {
    match node {
        AppNode::Terminal { address } => Ok(Node::Pane {
            pane_id: leaf(address)?,
            label: None,
        }),
        AppNode::Split {
            direction,
            children,
        } => {
            let mut rest = children.iter();
            let (first, first_weight) = rest.next().context("empty app layout split")?;
            let mut folded = to_binary(first, leaf)?;
            let mut spanned = *first_weight;
            for (child, weight) in rest {
                folded = Node::Split {
                    direction: *direction,
                    ratio: spanned / (spanned + weight),
                    first: Box::new(folded),
                    second: Box::new(to_binary(child, leaf)?),
                };
                spanned += weight;
            }
            Ok(folded)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_labels_drop_the_number_prefix_only() {
        assert_eq!(space_name("[3] work"), "work");
        assert_eq!(space_name("[12] a b"), "a b");
        assert_eq!(space_name("api"), "api");
        assert_eq!(space_name("[x] api"), "[x] api");
        assert_eq!(space_name("[] api"), "[] api");
        assert_eq!(space_name("[3] "), "");
    }

    #[test]
    fn a_tab_label_of_only_its_number_is_unnamed() {
        assert_eq!(tab_name("2"), None);
        assert_eq!(tab_name(""), None);
        assert_eq!(tab_name("build"), Some("build".into()));
        assert_eq!(tab_name("v2"), Some("v2".into()));
        // What the TUI's API reports: the number, then the name.
        assert_eq!(tab_name("[1] main"), Some("main".into()));
        assert_eq!(tab_name("[2] ! bash"), Some("! bash".into()));
        assert_eq!(tab_name("[3] 7"), None);
    }

    #[test]
    fn the_wrapper_passes_cwd_and_argv_as_parameters() {
        let program = Launch {
            cwd: "/srv/a b".into(),
            argv: Some(vec!["claude".into(), "--resume".into(), "x".into()]),
        };
        assert_eq!(
            wrapped(&program),
            [
                "sh",
                "-c",
                platform::LOGIN_PANE,
                "/srv/a b",
                "claude",
                "--resume",
                "x"
            ]
        );
        let shell = Launch {
            cwd: "/srv".into(),
            argv: None,
        };
        assert_eq!(wrapped(&shell), ["sh", "-c", CD_SHELL, "/srv"]);
    }

    #[test]
    fn the_wrapper_lands_in_a_folder_with_spaces_dollars_and_quotes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("x y$z'q\"");
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let home = root.path().canonicalize().unwrap();
        let gone = root.path().join("gone");
        // A folder that is gone leaves the shell in the home folder, without
        // the program.
        for (cwd, argv, lands) in [
            (&dir, Some(vec!["pwd".to_string(), "-P".to_string()]), &dir),
            (&dir, None, &dir),
            (
                &gone,
                Some(vec!["pwd".to_string(), "-P".to_string()]),
                &home,
            ),
            (&gone, None, &home),
        ] {
            let launch = Launch {
                cwd: cwd.to_str().unwrap().into(),
                argv: argv.clone(),
            };
            let argv = wrapped(&launch);
            // The shell left in the pane prints its folder too, from stdin.
            let output = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .env_clear()
                .env("HOME", root.path())
                .env("PATH", "/usr/bin:/bin")
                .env("SHELL", "/bin/sh")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(b"pwd -P\n")?;
                    child.wait_with_output()
                })
                .unwrap();
            assert!(output.status.success());
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                stdout.lines().collect::<Vec<_>>().len(),
                if launch.argv.is_some() && cwd == &dir {
                    2
                } else {
                    1
                },
                "{launch:?}: {stdout}"
            );
            assert_eq!(stdout.lines().next(), lands.to_str(), "{launch:?}");
        }
    }

    #[test]
    fn a_folder_is_shown_without_the_removed_mark() {
        assert_eq!(shown_folder("/p/gone (deleted)"), "/p/gone");
        assert_eq!(shown_folder("/p/build"), "/p/build");
    }

    fn block(terminal: &str, weight: Option<f64>) -> serde_json::Value {
        let mut value = json!({"type": "block", "id": format!("b{terminal}"), "content": {"terminal": terminal}});
        if let Some(weight) = weight {
            value["size"] = json!({"weight": weight});
        }
        value
    }

    fn convert(value: &serde_json::Value) -> (Option<Node>, usize) {
        let mut mods = 0;
        let node = parse_app_node(value, 0, &mut mods).unwrap();
        let node = node.map(|(node, _)| {
            to_binary(&node, &mut |address: &str| Ok(format!("w:p{address}"))).unwrap()
        });
        (node, mods)
    }

    fn pane(id: &str) -> Node {
        Node::Pane {
            pane_id: id.into(),
            label: None,
        }
    }

    #[test]
    fn a_row_folds_left_with_weight_ratios() {
        let row = json!({"type": "row", "id": "r", "children": [
            block("1", Some(2.0)), block("2", Some(1.0)), block("3", None)
        ]});
        let (node, mods) = convert(&row);
        assert_eq!(mods, 0);
        assert_eq!(
            node.unwrap(),
            Node::Split {
                direction: Direction::Right,
                ratio: 0.75,
                first: Box::new(Node::Split {
                    direction: Direction::Right,
                    ratio: 2.0 / 3.0,
                    first: Box::new(pane("w:p1")),
                    second: Box::new(pane("w:p2")),
                }),
                second: Box::new(pane("w:p3")),
            }
        );
    }

    #[test]
    fn columns_nest_and_single_children_collapse() {
        let tree = json!({"type": "row", "id": "r", "children": [
            block("1", None),
            {"type": "column", "id": "c", "size": {"weight": 3.0}, "children": [
                block("2", None),
                {"type": "row", "id": "only", "children": [block("3", None)]}
            ]}
        ]});
        let (node, _) = convert(&tree);
        assert_eq!(
            node.unwrap(),
            Node::Split {
                direction: Direction::Right,
                ratio: 0.25,
                first: Box::new(pane("w:p1")),
                second: Box::new(Node::Split {
                    direction: Direction::Down,
                    ratio: 0.5,
                    first: Box::new(pane("w:p2")),
                    second: Box::new(pane("w:p3")),
                }),
            }
        );
    }

    #[test]
    fn mod_blocks_are_counted_and_left_out() {
        let tree = json!({"type": "row", "id": "r", "children": [
            block("1", None),
            {"type": "block", "id": "m", "content": {"mod": "files"}}
        ]});
        let (node, mods) = convert(&tree);
        assert_eq!(mods, 1);
        assert_eq!(node.unwrap(), pane("w:p1"));
        let only_mods = json!({"type": "column", "id": "c", "children": [
            {"type": "block", "id": "m", "content": {}}
        ]});
        assert_eq!(convert(&only_mods), (None, 1));
    }

    #[test]
    fn unknown_nodes_and_bad_weights_are_refused() {
        let mut mods = 0;
        let stack = json!({"type": "stack", "id": "s", "children": []});
        assert!(parse_app_node(&stack, 0, &mut mods).is_err());
        assert!(parse_app_node(&block("1", Some(0.0)), 0, &mut mods).is_err());
        assert!(parse_app_node(&block("1", Some(-1.0)), 0, &mut mods).is_err());
    }

    #[test]
    fn first_pane_id_is_the_leftmost_leaf() {
        let node = Node::Split {
            direction: Direction::Down,
            ratio: 0.5,
            first: Box::new(Node::Split {
                direction: Direction::Right,
                ratio: 0.5,
                first: Box::new(pane("w:p7")),
                second: Box::new(pane("w:p8")),
            }),
            second: Box::new(pane("w:p9")),
        };
        assert_eq!(node.first_pane_id(), "w:p7");
    }
}
