//! `kj price` — what a model's tokens cost.
//!
//! Prices are an append-only history in `kernel_db` (`model_prices`): a change
//! adds a row, and the row in force when a turn started prices that turn.
//! Rates are dollars per million tokens, written as decimals (`3`, `0.30`,
//! `3.75`) and stored as integer micro-dollars. `list`, `history` and
//! `tool-history` read; `set` and `set-tool` need the `config-write`
//! capability, which the root, director, console and mcp loadouts hold,
//! because a price changes what every cost figure says.
//!
//! A model with no price has an unknown cost. It is shown as unknown and never
//! as free: a free model is a price of zero (`set --free`).
//!
//! A tool the provider runs and bills per call (a server-side web search) is
//! priced apart, in dollars per call, in `tool_prices`: `set-tool` takes a
//! backend and a tool name where `set` takes a model and per-million-token
//! rates. A tool use with no price counts as a call with no price.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use crate::kernel_db::{ModelPriceRow, NewModelPrice, NewToolPrice, ToolPriceRow};

use super::effect::{Classify, Effect};
use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "price",
    about = "Prices: what a model's tokens and a provider-billed tool call cost, as an append-only history",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct PriceArgs {
    #[command(subcommand)]
    command: PriceCommand,
}

#[derive(Subcommand, Debug)]
enum PriceCommand {
    /// List the price now in force for every priced model and every priced tool.
    #[command(alias = "ls")]
    List,
    /// Show every price a model has had, oldest first.
    History {
        /// Model: `<backend>/<model>` or a model alias
        model: String,
    },
    /// Set a model's price from now on.
    ///
    /// Adds a row to the history. Nothing is rewritten: turns already run keep
    /// the price in force when they started.
    ///
    /// Rates are dollars per million tokens. A cache rate you leave out is
    /// unknown: a cost that needs it is shown as unknown, not as a low number.
    /// Anthropic bills a cache read at 0.1x the input rate, a 5-minute cache
    /// write at 1.25x and a 1-hour cache write at 2x.
    Set {
        /// Model: `<backend>/<model>` or a model alias
        model: String,
        /// Input rate, dollars per million tokens (e.g. 3 or 0.80); required unless --free
        #[arg(long, conflicts_with = "free")]
        input: Option<String>,
        /// Output rate, dollars per million tokens; required unless --free
        #[arg(long, conflicts_with = "free")]
        output: Option<String>,
        /// Cache-read rate, dollars per million tokens
        #[arg(long, conflicts_with = "free")]
        cache_read: Option<String>,
        /// 5-minute cache-write rate, dollars per million tokens
        #[arg(long, conflicts_with = "free")]
        cache_write_5m: Option<String>,
        /// 1-hour cache-write rate, dollars per million tokens
        #[arg(long, conflicts_with = "free")]
        cache_write_1h: Option<String>,
        /// Every rate is zero: a model that costs nothing, such as a local one
        #[arg(long)]
        free: bool,
        /// Take effect at this instant, in unix milliseconds (default: now)
        #[arg(long)]
        from: Option<i64>,
        /// Where the numbers came from, kept with the row
        #[arg(long)]
        source: Option<String>,
    },
    /// Show every price a provider-billed tool has had, oldest first.
    ToolHistory {
        /// Backend name, as `kj backend list` shows it
        backend: String,
        /// Tool name the provider reports, such as `web_search`
        tool: String,
    },
    /// Set the price of a tool the provider runs and bills per call, from now on.
    ///
    /// A server-side web search is billed per call, so tokens cannot price it.
    /// Adds a row to the history; turns already run keep the price in force
    /// when they started. A tool use with no price is shown as a call with no
    /// price, never as free: use `--free` for a tool that costs nothing.
    SetTool {
        /// Backend name, as `kj backend list` shows it
        backend: String,
        /// Tool name the provider reports, such as `web_search`
        tool: String,
        /// Price of one call, dollars per call (e.g. 0.01); required unless --free
        #[arg(long, conflicts_with = "free")]
        per_call: Option<String>,
        /// The tool costs nothing: a price of zero
        #[arg(long)]
        free: bool,
        /// Take effect at this instant, in unix milliseconds (default: now)
        #[arg(long)]
        from: Option<i64>,
        /// Where the number came from, kept with the row
        #[arg(long)]
        source: Option<String>,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_price(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<PriceArgs>();
        }
        let parsed = match PriceArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj price: {e}"));
            }
        };
        match parsed.command {
            PriceCommand::List => self.price_list(),
            PriceCommand::History { model } => match self.price_target(&model).await {
                Ok((provider, model)) => self.price_history(&provider, &model),
                Err(e) => KjResult::Err(e),
            },
            PriceCommand::Set { model, input, output, cache_read, cache_write_5m, cache_write_1h, free, from, source } => {
                if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "price") {
                    return denied;
                }
                let (provider, model) = match self.price_target(&model).await {
                    Ok(target) => target,
                    Err(e) => return KjResult::Err(e),
                };
                let rates = match PriceFlags::parse(free, input, output, cache_read, cache_write_5m, cache_write_1h) {
                    Ok(rates) => rates,
                    Err(e) => return KjResult::Err(format!("kj price set: {e}")),
                };
                self.price_set(provider, model, rates, from, source, caller)
            }
            PriceCommand::ToolHistory { backend, tool } => match self.price_backend(&backend) {
                Ok(provider) => self.price_tool_history(&provider, &tool),
                Err(e) => KjResult::Err(e),
            },
            PriceCommand::SetTool { backend, tool, per_call, free, from, source } => {
                if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "price") {
                    return denied;
                }
                let provider = match self.price_backend(&backend) {
                    Ok(provider) => provider,
                    Err(e) => return KjResult::Err(e),
                };
                let per_call = match (free, per_call) {
                    (true, _) => 0,
                    (false, Some(text)) => match parse_dollars(&text, "dollars per call, like 0.01") {
                        Ok(micros) => micros,
                        Err(e) => return KjResult::Err(format!("kj price set-tool: --per-call: {e}")),
                    },
                    (false, None) => return KjResult::Err("kj price set-tool: --per-call is required (or --free)".into()),
                };
                self.price_set_tool(provider, tool, per_call, from, source, caller)
            }
        }
    }

    /// Resolve `<backend>/<model>` or a model alias to the `(provider, model)`
    /// strings a turn records. The backend must exist; the model is taken as given.
    async fn price_target(&self, spec: &str) -> Result<(String, String), String> {
        let registry = self.kernel().llm().read().await;
        let (provider, model) = match registry.resolve_alias(spec) {
            Some((backend, model)) => (backend.to_string(), model.to_string()),
            None => match spec.split_once('/') {
                Some((backend, model)) if !backend.is_empty() && !model.is_empty() => {
                    (backend.to_string(), model.to_string())
                }
                _ => {
                    return Err(format!(
                        "kj price: '{spec}' is not a model: write <backend>/<model> or an alias \
                         (`kj models` lists them)"
                    ));
                }
            },
        };
        self.price_backend(&provider).map(|provider| (provider, model))
    }

    /// The backend name a price is keyed by, checked to exist.
    fn price_backend(&self, name: &str) -> Result<String, String> {
        match self.kernel_db().lock().get_backend_by_name(name) {
            Ok(Some(_)) => Ok(name.to_string()),
            Ok(None) => Err(format!("kj price: no backend named '{name}' (`kj backend list`)")),
            Err(e) => Err(format!("kj price: {e}")),
        }
    }

    /// What one context's turns have cost, each priced by the price in force
    /// when it started. `None` when the context has recorded no calls.
    pub(crate) async fn context_cost(
        &self,
        context: kaijutsu_types::ContextId,
    ) -> Result<Option<crate::pricing::ContextCost>, String> {
        use crate::pricing::InputStyle;
        let styles: std::collections::HashMap<String, InputStyle> = self
            .kernel()
            .llm()
            .read()
            .await
            .backend_configs()
            .iter()
            .map(|b| (b.name.clone(), InputStyle::for_kind(&b.kind)))
            .collect();
        let db = self.kernel_db().lock();
        let mut costs = crate::pricing::context_costs(&db, Some(context), |p| styles.get(p).copied())
            .map_err(|e| format!("context cost unreadable: {e}"))?;
        Ok(costs.remove(&context))
    }

    fn price_list(&self) -> KjResult {
        let now = kaijutsu_types::now_millis() as i64;
        let db = self.kernel_db().lock();
        let rows = match db.list_model_prices(None) {
            Ok(rows) => rows,
            Err(e) => return KjResult::Err(format!("kj price list: {e}")),
        };
        let mut models: Vec<(String, String)> = rows.iter().map(|r| (r.provider.clone(), r.model.clone())).collect();
        models.dedup();
        let mut lines = Vec::new();
        let mut names = Vec::new();
        for (provider, model) in models {
            match db.effective_model_price(&provider, &model, now) {
                Ok(Some(price)) => {
                    lines.push(format!("{provider}/{model}  {}", describe_rates(&price)));
                    names.push(serde_json::Value::String(format!("{provider}/{model}")));
                }
                Ok(None) => {}
                Err(e) => return KjResult::Err(format!("kj price list: {e}")),
            }
        }
        let tools = match db.list_tool_prices(None) {
            Ok(rows) => rows,
            Err(e) => return KjResult::Err(format!("kj price list: {e}")),
        };
        let mut tool_keys: Vec<(String, String)> = tools.iter().map(|r| (r.provider.clone(), r.tool.clone())).collect();
        tool_keys.dedup();
        for (provider, tool) in tool_keys {
            match db.effective_tool_price(&provider, &tool, now) {
                Ok(Some(price)) => {
                    lines.push(format!("{provider} tool {tool}  {}", describe_tool_rate(&price)));
                    names.push(serde_json::Value::String(format!("tool:{provider}/{tool}")));
                }
                Ok(None) => {}
                Err(e) => return KjResult::Err(format!("kj price list: {e}")),
            }
        }
        if lines.is_empty() {
            return KjResult::ok_with_data(
                "Nothing has a price: every cost is unknown. Set a model's with `kj price set <backend>/<model> --input <$/M> --output <$/M>` (`--free` for a local model), a provider-billed tool's with `kj price set-tool <backend> <tool> --per-call <$>`.",
                serde_json::Value::Array(names),
            );
        }
        KjResult::ok_with_data(lines.join("\n"), serde_json::Value::Array(names))
    }

    fn price_history(&self, provider: &str, model: &str) -> KjResult {
        let rows = match self.kernel_db().lock().list_model_prices(Some((provider, model))) {
            Ok(rows) => rows,
            Err(e) => return KjResult::Err(format!("kj price history: {e}")),
        };
        if rows.is_empty() {
            return KjResult::ok(format!("{provider}/{model} has no price: its cost is unknown."));
        }
        let lines: Vec<String> = rows
            .iter()
            .map(|r| {
                let source = if r.source.is_empty() { String::new() } else { format!("  [{}]", r.source) };
                format!("#{} from {}  {}{source}", r.price_id, r.effective_from, describe_rates(r))
            })
            .collect();
        KjResult::ok(lines.join("\n"))
    }

    fn price_tool_history(&self, provider: &str, tool: &str) -> KjResult {
        let rows = match self.kernel_db().lock().list_tool_prices(Some((provider, tool))) {
            Ok(rows) => rows,
            Err(e) => return KjResult::Err(format!("kj price tool-history: {e}")),
        };
        if rows.is_empty() {
            return KjResult::ok(format!("{provider} tool {tool} has no price: its uses are counted as calls with no price."));
        }
        let lines: Vec<String> = rows
            .iter()
            .map(|r| {
                let source = if r.source.is_empty() { String::new() } else { format!("  [{}]", r.source) };
                format!("#{} from {}  {}{source}", r.price_id, r.effective_from, describe_tool_rate(r))
            })
            .collect();
        KjResult::ok(lines.join("\n"))
    }

    fn price_set_tool(
        &self,
        provider: String,
        tool: String,
        per_call: i64,
        from: Option<i64>,
        source: Option<String>,
        caller: &KjCaller,
    ) -> KjResult {
        let effective_from = from.unwrap_or_else(|| kaijutsu_types::now_millis() as i64);
        let price = NewToolPrice {
            provider: provider.clone(),
            tool: tool.clone(),
            effective_from,
            per_call,
            source: source.unwrap_or_default(),
            set_by: Some(caller.actor_id),
        };
        let db = self.kernel_db().lock();
        let id = match db.insert_tool_price(&price) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj price set-tool: {e}")),
        };
        match db.get_tool_price(id) {
            Ok(Some(row)) => KjResult::ok(format!("{provider} tool {tool} priced from {effective_from}: {}", describe_tool_rate(&row))),
            Ok(None) => KjResult::Err(format!("kj price set-tool: price #{id} was written but cannot be read back")),
            Err(e) => KjResult::Err(format!("kj price set-tool: {e}")),
        }
    }

    fn price_set(
        &self,
        provider: String,
        model: String,
        rates: PriceFlags,
        from: Option<i64>,
        source: Option<String>,
        caller: &KjCaller,
    ) -> KjResult {
        let effective_from = from.unwrap_or_else(|| kaijutsu_types::now_millis() as i64);
        let price = NewModelPrice {
            provider: provider.clone(),
            model: model.clone(),
            effective_from,
            input: rates.input,
            output: rates.output,
            cache_read: rates.cache_read,
            cache_write_5m: rates.cache_write_5m,
            cache_write_1h: rates.cache_write_1h,
            source: source.unwrap_or_default(),
            set_by: Some(caller.actor_id),
        };
        let db = self.kernel_db().lock();
        let id = match db.insert_model_price(&price) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj price set: {e}")),
        };
        match db.get_model_price(id) {
            Ok(Some(row)) => KjResult::ok(format!("{provider}/{model} priced from {effective_from}: {}", describe_rates(&row))),
            Ok(None) => KjResult::Err(format!("kj price set: price #{id} was written but cannot be read back")),
            Err(e) => KjResult::Err(format!("kj price set: {e}")),
        }
    }
}

/// The rates a `set` carries, in micro-dollars per million tokens.
struct PriceFlags {
    input: i64,
    output: i64,
    cache_read: Option<i64>,
    cache_write_5m: Option<i64>,
    cache_write_1h: Option<i64>,
}

impl PriceFlags {
    fn parse(
        free: bool,
        input: Option<String>,
        output: Option<String>,
        cache_read: Option<String>,
        cache_write_5m: Option<String>,
        cache_write_1h: Option<String>,
    ) -> Result<Self, String> {
        if free {
            return Ok(Self { input: 0, output: 0, cache_read: Some(0), cache_write_5m: Some(0), cache_write_1h: Some(0) });
        }
        let required = |flag: &str, value: Option<String>| -> Result<i64, String> {
            let text = value.ok_or_else(|| format!("--{flag} is required (or --free)"))?;
            parse_dollars_per_million(&text).map_err(|e| format!("--{flag}: {e}"))
        };
        let optional = |flag: &str, value: Option<String>| -> Result<Option<i64>, String> {
            value.map(|text| parse_dollars_per_million(&text).map_err(|e| format!("--{flag}: {e}"))).transpose()
        };
        Ok(Self {
            input: required("input", input)?,
            output: required("output", output)?,
            cache_read: optional("cache-read", cache_read)?,
            cache_write_5m: optional("cache-write-5m", cache_write_5m)?,
            cache_write_1h: optional("cache-write-1h", cache_write_1h)?,
        })
    }
}

/// `3.75` dollars per million tokens as micro-dollars, exactly: digits with
/// at most six decimals, no sign, no exponent. Anything finer would be lost.
fn parse_dollars_per_million(text: &str) -> Result<i64, String> {
    parse_dollars(text, "dollars per million tokens, like 3 or 0.30")
}

/// `text` dollars as micro-dollars, exactly; `unit` says what the number is
/// for the error message.
fn parse_dollars(text: &str, unit: &str) -> Result<i64, String> {
    let bad = || format!("'{text}' is not a rate: write {unit}");
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) || (text.contains('.') && fraction.is_empty()) {
        return Err(bad());
    }
    if fraction.len() > 6 {
        return Err(format!("'{text}' has more than six decimals, finer than a micro-dollar"));
    }
    let whole: i64 = whole.parse().map_err(|_| bad())?;
    let fraction: i64 = format!("{fraction:0<6}").parse().map_err(|_| bad())?;
    whole
        .checked_mul(1_000_000)
        .and_then(|w| w.checked_add(fraction))
        .ok_or_else(|| format!("'{text}' is too large"))
}

/// Micro-dollars as dollars, at least two decimals:
/// `3_000_000` is `$3.00`, `75_000` is `$0.075`.
fn dollars(micros: i64) -> String {
    let mut fraction = format!("{:06}", micros % 1_000_000);
    while fraction.len() > 2 && fraction.ends_with('0') {
        fraction.pop();
    }
    format!("${}.{fraction}", micros / 1_000_000)
}

fn describe_tool_rate(price: &ToolPriceRow) -> String {
    format!("{} per call", dollars(price.per_call))
}

fn describe_rates(price: &ModelPriceRow) -> String {
    let rate = |label: &str, value: Option<i64>| match value {
        Some(micros) => format!("{label} {}", dollars(micros)),
        None => format!("{label} unknown"),
    };
    format!(
        "in {}  out {}  {}  {}  {}  per million tokens",
        dollars(price.input),
        dollars(price.output),
        rate("cache-read", price.cache_read),
        rate("write-5m", price.cache_write_5m),
        rate("write-1h", price.cache_write_1h),
    )
}

impl Classify for PriceArgs {
    fn effect(&self) -> Effect {
        match self.command {
            PriceCommand::List | PriceCommand::History { .. } | PriceCommand::ToolHistory { .. } => Effect::Read,
            PriceCommand::Set { .. } | PriceCommand::SetTool { .. } => Effect::Write,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{caller_with_context, register_context, test_caller, test_dispatcher};
    use super::*;
    use kaijutsu_types::PrincipalId;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    async fn seeded() -> KjDispatcher {
        let d = test_dispatcher().await;
        {
            let db = d.kernel_db().lock();
            crate::test_backends::example_backends(&db);
        }
        d.reload_llm_registry().await.unwrap();
        d
    }

    async fn run(d: &KjDispatcher, parts: &[&str]) -> KjResult {
        d.dispatch(&argv(parts), &test_caller()).await
    }

    /// Dollars per million tokens, written as decimals, are stored exactly as
    /// micro-dollars, and set appends rather than replaces.
    #[tokio::test]
    async fn set_stores_decimal_rates_exactly_and_appends() {
        let d = seeded().await;
        let r = run(&d, &["price", "set", "anthropic/claude-haiku-5-5", "--input", "1", "--output", "5",
            "--cache-read", "0.10", "--cache-write-5m", "1.25", "--cache-write-1h", "2", "--source", "docs page"]).await;
        assert!(r.is_ok(), "{}", r.message());
        let r = run(&d, &["price", "set", "anthropic/claude-haiku-5-5", "--input", "0.8", "--output", "4"]).await;
        assert!(r.is_ok(), "{}", r.message());
        let rows = d.kernel_db().lock().list_model_prices(Some(("anthropic", "claude-haiku-5-5"))).unwrap();
        assert_eq!(rows.len(), 2, "the second set appends");
        let first = &rows[0];
        assert_eq!(
            (first.input, first.output, first.cache_read, first.cache_write_5m, first.cache_write_1h, first.source.as_str()),
            (1_000_000, 5_000_000, Some(100_000), Some(1_250_000), Some(2_000_000), "docs page")
        );
        assert_eq!((rows[1].input, rows[1].cache_read), (800_000, None), "an omitted cache rate is unknown");
    }

    /// Bad input fails loudly: a rate with more than six decimals, a negative
    /// one, a typo'd backend, and --free alongside a rate.
    #[tokio::test]
    async fn set_refuses_malformed_rates_and_unknown_backends() {
        let d = seeded().await;
        for bad in [
            &["price", "set", "anthropic/m", "--input", "0.1234567", "--output", "1"][..],
            &["price", "set", "anthropic/m", "--input", "-1", "--output", "1"][..],
            &["price", "set", "anthropic/m", "--input", "abc", "--output", "1"][..],
            &["price", "set", "nosuchbackend/m", "--input", "1", "--output", "1"][..],
            &["price", "set", "anthropic/m", "--free", "--input", "1"][..],
            &["price", "set", "anthropic", "--input", "1", "--output", "1"][..],
            &["price", "set", "anthropic/m", "--output", "1"][..],
            &["price", "set", "anthropic/m"][..],
        ] {
            let r = run(&d, bad).await;
            assert!(!r.is_ok(), "{bad:?} should fail: {}", r.message());
        }
        assert!(d.kernel_db().lock().list_model_prices(None).unwrap().is_empty());
    }

    /// A free model is a price of zero; it is not the same as no price.
    #[tokio::test]
    async fn free_is_a_zero_price_and_list_says_unknown_for_the_rest() {
        let d = seeded().await;
        let r = run(&d, &["price", "set", "ollama/llama", "--free"]).await;
        assert!(r.is_ok(), "{}", r.message());
        let row = d.kernel_db().lock().effective_model_price("ollama", "llama", i64::MAX).unwrap().unwrap();
        assert_eq!((row.input, row.output, row.cache_read, row.cache_write_5m, row.cache_write_1h),
            (0, 0, Some(0), Some(0), Some(0)));
        let list = run(&d, &["price", "list"]).await;
        assert!(list.message().contains("ollama/llama"), "{}", list.message());
        let history = run(&d, &["price", "history", "anthropic/never-priced"]).await;
        assert!(history.message().contains("no price"), "{}", history.message());
    }

    /// A model alias names its backend and model.
    #[tokio::test]
    async fn an_alias_resolves_to_its_backend_and_model() {
        let d = seeded().await;
        let made = run(&d, &["alias", "set", "opus", "--backend", "anthropic", "--model", "claude-opus-5"]).await;
        assert!(made.is_ok(), "{}", made.message());
        let r = run(&d, &["price", "set", "opus", "--input", "5", "--output", "25"]).await;
        assert!(r.is_ok(), "{}", r.message());
        let row = d.kernel_db().lock().effective_model_price("anthropic", "claude-opus-5", i64::MAX).unwrap();
        assert_eq!(row.map(|p| p.input), Some(5_000_000));
    }

    #[test]
    fn rates_parse_exactly_and_print_with_at_least_two_decimals() {
        for (text, micros) in [("3", 3_000_000), ("0.30", 300_000), ("3.75", 3_750_000), ("0.075", 75_000), ("0", 0), ("12.000001", 12_000_001)] {
            assert_eq!(parse_dollars_per_million(text), Ok(micros), "{text}");
        }
        for bad in ["", ".5", "5.", "-1", "1e3", "1,5", "0.1234567", "1 ", "9223372036854775807"] {
            assert!(parse_dollars_per_million(bad).is_err(), "{bad:?} should be refused");
        }
        for (micros, text) in [(3_000_000, "$3.00"), (300_000, "$0.30"), (75_000, "$0.075"), (0, "$0.00"), (12_000_001, "$12.000001")] {
            assert_eq!(dollars(micros), text);
        }
    }

    #[tokio::test]
    async fn help_states_the_unit_and_what_an_omitted_rate_means() {
        let d = seeded().await;
        let top = run(&d, &["price", "help"]).await;
        let set = run(&d, &["price", "set", "--help"]).await;
        assert!(top.message().contains("Set a model's price from now on\n"), "{}", top.message());
        assert!(set.message().contains("dollars per million tokens"), "{}", set.message());
        assert!(set.message().contains("unknown"), "{}", set.message());
    }

    /// The shipped gate defaults let the operator seats set a tool price as
    /// they set a model price.
    #[test]
    fn the_shipped_gate_allows_set_tool_wherever_it_allows_set() {
        let gate: toml::Table = crate::config_seed::DEFAULT_GATE_CONFIG.parse().unwrap();
        let allow = |section: &toml::Value| -> Vec<String> {
            section["allow"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
        };
        for (name, section) in [("rc.banto", &gate["rc"]["banto"]), ("root", &gate["root"])] {
            let allowed = allow(section);
            assert!(allowed.contains(&"kj price set".to_string()), "{name} allows kj price set");
            assert!(allowed.contains(&"kj price set-tool".to_string()), "{name}: {allowed:?}");
        }
    }

    /// A tool price is dollars per call stored exactly, appends, lists beside
    /// the model prices, and `--free` is a price of zero.
    #[tokio::test]
    async fn set_tool_stores_dollars_per_call_exactly_and_appends() {
        let d = seeded().await;
        let r = run(&d, &["price", "set-tool", "anthropic", "web_search", "--per-call", "0.01", "--source", "docs page"]).await;
        assert!(r.is_ok(), "{}", r.message());
        assert!(r.message().contains("$0.01 per call"), "{}", r.message());
        let r = run(&d, &["price", "set-tool", "anthropic", "web_search", "--per-call", "0.0075"]).await;
        assert!(r.is_ok(), "{}", r.message());
        let r = run(&d, &["price", "set-tool", "ollama", "web_search", "--free"]).await;
        assert!(r.is_ok(), "{}", r.message());
        let db = d.kernel_db().lock();
        let rows = db.list_tool_prices(Some(("anthropic", "web_search"))).unwrap();
        assert_eq!(rows.iter().map(|r| r.per_call).collect::<Vec<_>>(), vec![10_000, 7_500], "the second set appends");
        assert_eq!(rows[0].source, "docs page");
        assert_eq!(db.effective_tool_price("ollama", "web_search", i64::MAX).unwrap().map(|p| p.per_call), Some(0));
        drop(db);
        let list = run(&d, &["price", "list"]).await;
        assert!(list.message().contains("anthropic tool web_search  $0.0075 per call"), "{}", list.message());
        let history = run(&d, &["price", "tool-history", "anthropic", "web_search"]).await;
        assert!(history.message().contains("$0.01 per call") && history.message().contains("[docs page]"), "{}", history.message());
        let none = run(&d, &["price", "tool-history", "anthropic", "web_extractor"]).await;
        assert!(none.message().contains("no price"), "{}", none.message());
    }

    /// Bad input fails loudly and writes nothing.
    #[tokio::test]
    async fn set_tool_refuses_malformed_rates_and_unknown_backends() {
        let d = seeded().await;
        for bad in [
            &["price", "set-tool", "anthropic", "web_search"][..],
            &["price", "set-tool", "anthropic", "web_search", "--per-call", "-1"][..],
            &["price", "set-tool", "anthropic", "web_search", "--per-call", "0.1234567"][..],
            &["price", "set-tool", "anthropic", "web_search", "--per-call", "abc"][..],
            &["price", "set-tool", "anthropic", "web_search", "--free", "--per-call", "1"][..],
            &["price", "set-tool", "nosuchbackend", "web_search", "--per-call", "1"][..],
            &["price", "set-tool", "anthropic", "", "--per-call", "1"][..],
        ] {
            let r = run(&d, bad).await;
            assert!(!r.is_ok(), "{bad:?} should fail: {}", r.message());
        }
        assert!(d.kernel_db().lock().list_tool_prices(None).unwrap().is_empty());
    }

    /// Setting a tool price needs `config-write`, like setting a model price.
    #[tokio::test]
    async fn set_tool_needs_config_write() {
        use crate::mcp::{Capability, ContextToolBinding};
        let d = seeded().await;
        let house = register_context(&d, Some("house"), None, PrincipalId::new());
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::House);
        d.kernel_db().lock().upsert_context_binding(house, &binding).unwrap();
        let denied = d.dispatch(&argv(&["price", "set-tool", "anthropic", "web_search", "--per-call", "1"]), &caller_with_context(house)).await;
        assert!(!denied.is_ok() && denied.message().contains("config-write"), "{}", denied.message());
        assert!(d.kernel_db().lock().list_tool_prices(None).unwrap().is_empty());
    }

    /// A seat without the house capability is refused the verb outright. A
    /// house seat reads freely, and setting needs `config-write` as well.
    #[tokio::test]
    async fn the_house_and_config_write_gates_stand_in_order() {
        use crate::mcp::{Capability, ContextToolBinding};
        let d = seeded().await;
        let seat = |label: &str, caps: &[Capability]| {
            let ctx = register_context(&d, Some(label), None, PrincipalId::new());
            let mut binding = ContextToolBinding::new();
            for cap in caps {
                binding.grant(cap.clone());
            }
            d.kernel_db().lock().upsert_context_binding(ctx, &binding).unwrap();
            caller_with_context(ctx)
        };
        let set = argv(&["price", "set", "anthropic/m", "--input", "1", "--output", "1"]);
        let list = argv(&["price", "list"]);

        let worker = seat("worker", &[Capability::Drift]);
        assert!(d.dispatch(&list, &worker).await.message().contains("not part of this seat's work"));

        let house = seat("house", &[Capability::House]);
        assert!(d.dispatch(&list, &house).await.is_ok());
        let denied = d.dispatch(&set, &house).await;
        assert!(!denied.is_ok() && denied.message().contains("config-write"), "{}", denied.message());

        let director = seat("director", &[Capability::House, Capability::ConfigWrite]);
        let done = d.dispatch(&set, &director).await;
        assert!(done.is_ok(), "{}", done.message());
    }
}
