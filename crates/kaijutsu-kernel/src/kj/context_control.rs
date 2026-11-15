//! `kj context control`: the ordered control vectors a context sends with
//! each turn on a council backend.
//!
//! The list lives in `context_controls`. A change applies at the next turn,
//! and a fork copies the list. Kaijutsu stores the vector's content id only;
//! the council server holds the vector. See `docs/council-backend.md`,
//! "Control lists".

use clap::Subcommand;
use gogi_proto::ContentId;
use gogi_proto::control::{Control, ControlOp};

use super::effect::{Classify, Effect};
use super::refs::resolve_context_arg;
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Subcommand, Debug)]
pub(crate) enum ContextControlCommand {
    /// Show the control list with each item's position, vector, op,
    /// strength, and layers. `rm` takes a position from this output.
    #[command(alias = "ls")]
    List {
        /// Context whose list to show. Defaults to the current context. Ids
        /// and labels come from `kj context list`.
        #[arg(long)]
        context: Option<String>,
    },
    /// Append a control item: `kj context control add blake3:<64 hex digits>
    /// --op ablate --strength 1`. The next turn on a council backend sends
    /// the list; any other backend refuses a turn while the list is not empty.
    Add {
        /// Content id of the control vector: `blake3:` and 64 lowercase hex
        /// digits. The council server holds the vector.
        vector: String,
        /// What the item does to the residual: add, ablate, or clamp.
        #[arg(long)]
        op: String,
        /// A finite number. Its scale is the vector's: `add` at 1 adds the
        /// vector as it is, and `ablate` at 1 removes the part along it.
        #[arg(long, allow_hyphen_values = true)]
        strength: f64,
        /// Limit the item to one layer, as `<layer>=<weight>`: the layer
        /// number in decimal and a finite weight the strength is multiplied
        /// by. Repeat the flag for more layers. Without it the item acts on
        /// every layer of the vector.
        #[arg(long = "layer", value_name = "LAYER=WEIGHT", allow_hyphen_values = true)]
        layers: Vec<String>,
        /// Context to change. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        #[arg(long)]
        context: Option<String>,
    },
    /// Remove the item at a position `list` shows. Later items move up one.
    #[command(alias = "remove")]
    Rm {
        /// Position of the item, counted from 0, as `kj context control
        /// list` prints it.
        position: usize,
        /// Context to change. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        #[arg(long)]
        context: Option<String>,
    },
    /// Remove every control item, so the next turn sends none.
    Clear {
        /// Context to change. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        #[arg(long)]
        context: Option<String>,
    },
}

impl ContextControlCommand {
    /// Whether the verb changes the list.
    pub(super) fn changes_list(&self) -> bool {
        !matches!(self, Self::List { .. })
    }
}

impl Classify for ContextControlCommand {
    fn effect(&self) -> Effect {
        match self {
            Self::List { .. } => Effect::Read,
            Self::Add { .. } | Self::Rm { .. } | Self::Clear { .. } => Effect::Write,
        }
    }
}

pub(super) fn op_name(op: ControlOp) -> &'static str {
    match op {
        ControlOp::Add => "add",
        ControlOp::Ablate => "ablate",
        ControlOp::Clamp => "clamp",
    }
}

fn parse_op(text: &str) -> Result<ControlOp, String> {
    match text {
        "add" => Ok(ControlOp::Add),
        "ablate" => Ok(ControlOp::Ablate),
        "clamp" => Ok(ControlOp::Clamp),
        other => Err(format!("--op is add, ablate, or clamp, not {other:?}")),
    }
}

/// One `--layer <layer>=<weight>` flag.
fn parse_layer(text: &str) -> Result<(u32, f64), String> {
    let Some((layer, weight)) = text.split_once('=') else {
        return Err(format!("--layer {text:?} is not <layer>=<weight>, such as 12=0.5"));
    };
    let number = (!layer.is_empty() && layer.bytes().all(|b| b.is_ascii_digit()))
        .then(|| layer.parse::<u32>().ok())
        .flatten()
        .ok_or_else(|| format!("--layer {text:?}: the layer {layer:?} is not a layer number in decimal, such as 12"))?;
    let weight = weight
        .parse::<f64>()
        .ok()
        .filter(|w| w.is_finite())
        .ok_or_else(|| format!("--layer {text:?}: the weight {weight:?} is not a finite number"))?;
    Ok((number, weight))
}

/// The item the `add` flags describe, validated as the council server
/// validates it.
fn build_item(vector: &str, op: &str, strength: f64, layers: &[String]) -> Result<Control, String> {
    let vector = ContentId::parse(vector).map_err(|e| format!("the vector: {e}"))?;
    let op = parse_op(op)?;
    let mut by_layer = std::collections::BTreeMap::new();
    for spec in layers {
        let (layer, weight) = parse_layer(spec)?;
        if by_layer.insert(layer, weight).is_some() {
            return Err(format!("--layer names layer {layer} twice; give each layer once"));
        }
    }
    let item = Control { vector, op, strength, layers: (!by_layer.is_empty()).then_some(by_layer) };
    item.check("the item").map_err(|e| e.message)?;
    Ok(item)
}

/// One item as `list` prints it, after its position.
pub(super) fn describe_item(item: &Control) -> String {
    let layers = match &item.layers {
        None => "all layers".to_string(),
        Some(layers) => {
            let pairs: Vec<String> = layers.iter().map(|(l, w)| format!("{l}={w}")).collect();
            format!("layers {}", pairs.join(", "))
        }
    };
    format!("{}  {}  strength {}  {layers}", item.vector.as_str(), op_name(item.op), item.strength)
}

pub(super) fn items_json(items: &[Control]) -> serde_json::Value {
    items
        .iter()
        .enumerate()
        .map(|(position, item)| {
            serde_json::json!({
                "position": position,
                "vector": item.vector.as_str(),
                "op": op_name(item.op),
                "strength": item.strength,
                "layers": item.layers.as_ref().map(|layers| {
                    layers.iter().map(|(l, w)| (l.to_string(), serde_json::json!(w))).collect::<serde_json::Map<_, _>>()
                }),
            })
        })
        .collect()
}

impl KjDispatcher {
    pub(super) fn context_control(&self, command: ContextControlCommand, caller: &KjCaller) -> KjResult {
        let (verb, context) = match &command {
            ContextControlCommand::List { context } => ("list", context),
            ContextControlCommand::Add { context, .. } => ("add", context),
            ContextControlCommand::Rm { context, .. } => ("rm", context),
            ContextControlCommand::Clear { context } => ("clear", context),
        };
        let fail = |message: String| KjResult::Err(format!("kj context control {verb}: {message}"));
        let db = self.kernel_db().lock();
        let target = match resolve_context_arg(context.as_deref(), caller, &db) {
            Ok(id) => id,
            Err(e) => return fail(e),
        };
        match command {
            ContextControlCommand::List { .. } => match db.list_context_controls(target) {
                Ok(items) if items.is_empty() => KjResult::ok_with_data("no control items".to_string(), serde_json::json!([])),
                Ok(items) => {
                    let text = items
                        .iter()
                        .enumerate()
                        .map(|(position, item)| format!("{position}  {}", describe_item(item)))
                        .collect::<Vec<_>>()
                        .join("\n");
                    KjResult::ok_with_data(text, items_json(&items))
                }
                Err(e) => fail(e.to_string()),
            },
            ContextControlCommand::Add { vector, op, strength, layers, .. } => {
                let item = match build_item(&vector, &op, strength, &layers) {
                    Ok(item) => item,
                    Err(e) => return fail(e),
                };
                match db.add_context_control(target, &item) {
                    Ok(position) => KjResult::ok(format!(
                        "added control {position}: {}\nThe next turn sends it; a backend that is not a council \
                         server whose model declares control_vectors refuses the turn.",
                        describe_item(&item)
                    )),
                    Err(e) => fail(e.to_string()),
                }
            }
            ContextControlCommand::Rm { position, .. } => match db.remove_context_control(target, position) {
                Ok(item) => KjResult::ok(format!("removed control {position}: {}", describe_item(&item))),
                Err(e) => fail(e.to_string()),
            },
            ContextControlCommand::Clear { .. } => match db.clear_context_controls(target) {
                Ok(0) => KjResult::ok("no control items to clear".to_string()),
                Ok(n) => KjResult::ok(format!("cleared {n} control item(s); the next turn sends none")),
                Err(e) => fail(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use kaijutsu_types::PrincipalId;

    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    fn id(c: char) -> String {
        format!("blake3:{}", c.to_string().repeat(64))
    }

    async fn run(d: &crate::kj::KjDispatcher, ctx: kaijutsu_types::ContextId, args: &[&str]) -> crate::kj::KjResult {
        let mut argv = vec![s("context"), s("control")];
        argv.extend(args.iter().map(|a| s(a)));
        d.dispatch(&argv, &caller_with_context(ctx)).await
    }

    #[tokio::test]
    async fn add_list_rm_clear_round_trip() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("ctl"), None, PrincipalId::new());

        let empty = run(&d, ctx, &["list"]).await;
        assert!(empty.is_ok() && empty.message() == "no control items", "{}", empty.message());

        let a = id('a');
        let b = id('b');
        let r = run(&d, ctx, &["add", &a, "--op", "ablate", "--strength", "1"]).await;
        assert!(r.is_ok(), "{}", r.message());
        assert!(r.message().contains("added control 0"), "{}", r.message());
        let r = run(&d, ctx, &["add", &b, "--op", "add", "--strength", "-0.5", "--layer", "12=1", "--layer", "3=0.25"]).await;
        assert!(r.is_ok(), "{}", r.message());

        let listed = run(&d, ctx, &["list"]).await;
        let text = listed.message();
        assert!(text.contains(&format!("0  {a}  ablate  strength 1  all layers")), "{text}");
        assert!(text.contains(&format!("1  {b}  add  strength -0.5  layers 3=0.25, 12=1")), "{text}");
        let crate::kj::KjResult::Ok { data: Some(data), .. } = &listed else { panic!("{listed:?}") };
        assert_eq!(data[1]["layers"]["12"], 1.0);
        assert_eq!(data[0]["layers"], serde_json::Value::Null);

        let r = run(&d, ctx, &["rm", "0"]).await;
        assert!(r.is_ok() && r.message().contains("removed control 0"), "{}", r.message());
        let listed = run(&d, ctx, &["list"]).await;
        let text = listed.message();
        assert!(text.starts_with(&format!("0  {b}")), "positions close up: {text}");
        let r = run(&d, ctx, &["rm", "5"]).await;
        assert!(!r.is_ok() && r.message().contains("no control at position 5"), "{}", r.message());

        let r = run(&d, ctx, &["clear"]).await;
        assert!(r.is_ok() && r.message().contains("cleared 1"), "{}", r.message());
        assert!(d.kernel_db().lock().list_context_controls(ctx).unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_refuses_bad_input_and_stores_nothing() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("ctl-bad"), None, PrincipalId::new());
        let good = id('a');
        let cases: [(&[&str], &str); 8] = [
            (&["add", &good, "--op", "erase", "--strength", "1"], "add, ablate, or clamp"),
            (&["add", &good, "--op", "add", "--strength", "NaN"], "finite"),
            (&["add", &good, "--op", "add", "--strength", "inf"], "finite"),
            (&["add", "blake3:abc", "--op", "add", "--strength", "1"], "hex digits"),
            (&["add", &"a".repeat(64), "--op", "add", "--strength", "1"], "no scheme"),
            (&["add", &good, "--op", "add", "--strength", "1", "--layer", "twelve=1"], "layer number"),
            (&["add", &good, "--op", "add", "--strength", "1", "--layer", "12"], "<layer>=<weight>"),
            (&["add", &good, "--op", "add", "--strength", "1", "--layer", "3=1", "--layer", "3=2"], "twice"),
        ];
        for (args, needle) in cases {
            let r = run(&d, ctx, args).await;
            assert!(!r.is_ok(), "{args:?} was accepted");
            assert!(r.message().contains(needle), "{args:?}: {}", r.message());
        }
        let r = run(&d, ctx, &["add", &good, "--op", "add", "--strength", "1", "--layer", "3=nan"]).await;
        assert!(!r.is_ok() && r.message().contains("finite"), "{}", r.message());
        assert!(d.kernel_db().lock().list_context_controls(ctx).unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_context_flag_names_the_target() {
        let d = test_dispatcher().await;
        let here = register_context(&d, Some("ctl-here"), None, PrincipalId::new());
        let there = register_context(&d, Some("ctl-there"), None, PrincipalId::new());
        let r = run(&d, here, &["add", &id('a'), "--op", "clamp", "--strength", "0", "--context", "ctl-there"]).await;
        assert!(r.is_ok(), "{}", r.message());
        assert!(d.kernel_db().lock().list_context_controls(here).unwrap().is_empty());
        assert_eq!(d.kernel_db().lock().list_context_controls(there).unwrap().len(), 1);
    }

    /// `kj context info` shows the list, so the setting is visible where the
    /// other per-context settings are.
    #[tokio::test]
    async fn info_shows_the_control_list() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("ctl-info"), None, PrincipalId::new());
        run(&d, ctx, &["add", &id('a'), "--op", "ablate", "--strength", "1"]).await;
        let info = d.dispatch(&[s("context"), s("info")], &caller_with_context(ctx)).await;
        assert!(info.is_ok(), "{}", info.message());
        assert!(info.message().contains("Control:") && info.message().contains(&id('a')), "{}", info.message());
        let crate::kj::KjResult::Ok { data: Some(data), .. } = &info else { panic!("{info:?}") };
        assert_eq!(data["control"][0]["op"], "ablate");
    }

    #[test]
    fn list_is_a_read_and_the_rest_write() {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrap {
            #[command(subcommand)]
            c: ContextControlCommand,
        }
        let classify = |argv: &[&str]| Wrap::try_parse_from(std::iter::once("w").chain(argv.iter().copied())).unwrap().c.effect();
        assert_eq!(classify(&["list"]), Effect::Read);
        assert_eq!(classify(&["rm", "0"]), Effect::Write);
        assert_eq!(classify(&["clear"]), Effect::Write);
        let vector = id('a');
        assert_eq!(classify(&["add", &vector, "--op", "add", "--strength", "-1"]), Effect::Write);
    }
}
