//! Erratum sets E2 and E4: overrides of pinned negative expectations, written
//! from the REM-PARITY text alone by separate authors. The frozen sources are
//! never edited; each override names its case, variant and executor
//! observation, states the expectation it replaces, and is reported as ERRATUM.
//! E4's entries replace E2's for the vectors the owner's rulings Q9 and Q10
//! decide; each set is its own file, and no observation is overridden twice.
use super::*;

/// The erratum files, beside the frozen negative sources: E2, then E4.
pub fn paths() -> [PathBuf; 2] {
    [
        fixture_root().join("tape-images/negatives/erratum-e2.json"),
        fixture_root().join("tape-images/negatives/erratum-e4.json"),
    ]
}

/// One override of one executor observation.
#[derive(Clone, Debug)]
pub struct ErratumObservation {
    /// The author's name for the observation.
    pub author_observation: String,
    /// The compared outcome: the author's error, or `ACCEPTED` for none.
    pub outcome: String,
    /// The author's entry, transcribed verbatim.
    pub expected: Value,
}

/// One erratum entry for a case or variant.
#[derive(Clone, Debug)]
pub struct ErratumEntry {
    /// `negative-cases.json` or `negative-cases-supplement.json`.
    pub source: String,
    pub case: String,
    /// The variant name, or empty for none.
    pub variant: String,
    pub clear_freeze_blocker: bool,
    /// Why an off-tape or device-report case now executes.
    pub executes: Option<String>,
    /// The frozen expectation and pinned flag this entry replaces.
    pub replaces: Value,
    /// Overrides keyed by executor observation key.
    pub observations: BTreeMap<String, ErratumObservation>,
    /// Author outcomes that no executor run observes, with the reason.
    pub unobserved: Vec<Value>,
}

/// The parsed erratum set.
#[derive(Clone, Debug, Default)]
pub struct Errata {
    pub entries: Vec<ErratumEntry>,
}

/// The compared outcome of an author's entry: a Section 15 name, a scoped
/// metadata-unavailable outcome, or `ACCEPTED` when the author names no error.
fn outcome_of(expected: &Value) -> Result<String, String> {
    // Erratum set E4 states the outcome, and names the error without its epoch
    // (`SidecarMetadataUnavailable`); the outcome carries the epoch.
    if let Some(outcome) = expected.get("outcome").and_then(Value::as_str) {
        return match &expected["error"] {
            Value::Null => Ok("ACCEPTED".into()),
            Value::String(name) if name == "SidecarMetadataUnavailable" => {
                let epoch = outcome
                    .strip_prefix("SidecarMetadataUnavailable{epoch_id: ")
                    .and_then(|s| s.split_once('}'))
                    .and_then(|(epoch, _)| epoch.parse::<u64>().ok())
                    .ok_or_else(|| format!("E4 outcome without an epoch: {outcome}"))?;
                Ok(format!("SidecarMetadataUnavailable{{epoch_id: {epoch}}}"))
            }
            error => outcome_of(&json!({"error": error})),
        };
    }
    match &expected["error"] {
        Value::Null => Ok("ACCEPTED".into()),
        Value::String(name) => {
            let scoped = name
                .strip_prefix("SidecarMetadataUnavailable{epoch_id: ")
                .and_then(|s| s.strip_suffix('}'))
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|epoch| {
                    *name == format!("SidecarMetadataUnavailable{{epoch_id: {epoch}}}")
                });
            if scoped
                || matches!(
                    name.as_str(),
                    "SidecarParse"
                        | "ParityMapParse"
                        | "DirectoryInvalid"
                        | "BootstrapParse"
                        | "SchemeMismatch"
                        | "FilemarkMapReconstruct"
                        | "ResumeAppend"
                        | "TapeIo"
                        | "BotStructuralRecoveryRequired"
                        | "TerminalIndexReplicaParse"
                        | "TerminalIndexSeparationParse"
                )
            {
                Ok(name.clone())
            } else {
                Err(format!("unknown erratum outcome {name}"))
            }
        }
        other => Err(format!("erratum error must be a name or null, got {other}")),
    }
}

/// Parse the erratum set, failing closed on any unknown key, a duplicate
/// override, or an outcome the executor cannot identify.
pub fn parse(bytes: &[u8]) -> Result<Errata, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mut errata = Errata::default();
    let mut seen = BTreeSet::new();
    for entry in value["entries"]
        .as_array()
        .ok_or("erratum entries missing")?
    {
        for key in entry.as_object().ok_or("erratum entry object")?.keys() {
            if !matches!(
                key.as_str(),
                "source"
                    | "case"
                    | "variant"
                    | "clear_freeze_blocker"
                    | "executes"
                    | "replaces"
                    | "observations"
                    | "unobserved"
            ) {
                return Err(format!("unknown erratum key {key}"));
            }
        }
        let source = entry["source"]
            .as_str()
            .ok_or("erratum source")?
            .to_string();
        if !matches!(
            source.as_str(),
            "negative-cases.json" | "negative-cases-supplement.json"
        ) {
            return Err(format!("unknown erratum source {source}"));
        }
        let case = entry["case"].as_str().ok_or("erratum case")?.to_string();
        let variant = entry["variant"]
            .as_str()
            .ok_or("erratum variant")?
            .to_string();
        let mut observations = BTreeMap::new();
        for observation in entry["observations"]
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or("erratum observations missing or empty")?
        {
            let key = observation["observation"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("erratum observation key")?
                .to_string();
            if !seen.insert((source.clone(), case.clone(), variant.clone(), key.clone())) {
                return Err(format!(
                    "duplicate erratum override {case}/{variant} [{key}]"
                ));
            }
            let expected = observation["expected"].clone();
            for key in expected
                .as_object()
                .ok_or("erratum expected object")?
                .keys()
            {
                if !matches!(
                    key.as_str(),
                    "result" | "error" | "sections" | "quotes" | "open" | "outcome" | "derivation"
                ) {
                    return Err(format!("unknown author key {key}"));
                }
            }
            observations.insert(
                key,
                ErratumObservation {
                    author_observation: observation["author_observation"]
                        .as_str()
                        .ok_or("author observation")?
                        .to_string(),
                    outcome: outcome_of(&expected)?,
                    expected,
                },
            );
        }
        errata.entries.push(ErratumEntry {
            source,
            case,
            variant,
            clear_freeze_blocker: entry["clear_freeze_blocker"] == true,
            executes: entry["executes"].as_str().map(str::to_string),
            replaces: entry["replaces"].clone(),
            observations,
            unobserved: entry["unobserved"].as_array().cloned().unwrap_or_default(),
        });
    }
    Ok(errata)
}

/// Read the erratum sets from the fixture tree. No observation may be
/// overridden by two entries, in one set or across them.
pub fn load() -> Result<Errata, String> {
    let mut all = Errata::default();
    let mut seen = BTreeSet::new();
    for path in paths() {
        let set = parse(&fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?)?;
        for entry in &set.entries {
            for key in entry.observations.keys() {
                if !seen.insert((
                    entry.source.clone(),
                    entry.case.clone(),
                    entry.variant.clone(),
                    key.clone(),
                )) {
                    return Err(format!(
                        "erratum override in two sets: {}/{} [{key}]",
                        entry.case, entry.variant
                    ));
                }
            }
        }
        all.entries.extend(set.entries);
    }
    Ok(all)
}

impl Errata {
    /// The entry for one case or variant of one source.
    pub fn entry(&self, source: &str, case: &str, variant: &str) -> Option<&ErratumEntry> {
        self.entries
            .iter()
            .find(|e| e.source == source && e.case == case && e.variant == variant)
    }

    /// The override for one observation, if any.
    pub fn observation(
        &self,
        source: &str,
        case: &str,
        variant: &str,
        key: &str,
    ) -> Option<(&ErratumEntry, &ErratumObservation)> {
        self.entries
            .iter()
            .filter(|e| e.source == source && e.case == case && e.variant == variant)
            .find_map(|entry| Some((entry, entry.observations.get(key)?)))
    }

    /// Every entry must replace exactly the frozen expectation it names, so a
    /// drift in either file fails rather than silently re-pinning.
    pub fn check_against_sources(
        &self,
        negatives: &Value,
        supplement: &Value,
    ) -> Result<(), String> {
        for entry in &self.entries {
            let (expected, pinned) = if entry.source == "negative-cases.json" {
                let case = negatives["cases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["id"] == entry.case.as_str())
                    .ok_or_else(|| format!("erratum names unknown case {}", entry.case))?;
                if entry.variant.is_empty() {
                    (case["expected"].clone(), case["pinned"].clone())
                } else {
                    let variant = case["variants"]
                        .as_array()
                        .and_then(|v| v.iter().find(|v| v["variant"] == entry.variant.as_str()))
                        .ok_or_else(|| {
                            format!(
                                "erratum names unknown variant {}/{}",
                                entry.case, entry.variant
                            )
                        })?;
                    (
                        variant.get("expected").unwrap_or(&case["expected"]).clone(),
                        variant.get("pinned").unwrap_or(&case["pinned"]).clone(),
                    )
                }
            } else {
                let id = format!("{}/{}", entry.case, entry.variant);
                let variant = supplement["variants"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["id"] == id.as_str())
                    .ok_or_else(|| format!("erratum names unknown supplement variant {id}"))?;
                // `replaces` records one observation's frozen expectation, so an
                // entry overriding several could drift unchecked in the others.
                if entry.observations.len() != 1 {
                    return Err(format!(
                        "supplement erratum {id} overrides {} observations; it must override exactly one",
                        entry.observations.len()
                    ));
                }
                let key = entry.observations.keys().next().unwrap();
                let expected = if variant["expected"][key.as_str()].is_null() {
                    variant["expected"].clone()
                } else {
                    variant["expected"][key.as_str()].clone()
                };
                (expected, variant["pinned"].clone())
            };
            if entry.replaces != json!({"expected": expected, "pinned": pinned}) {
                return Err(format!(
                    "erratum {}/{} replaces {}, but the frozen source now reads expected {expected}, pinned {pinned}",
                    entry.case, entry.variant, entry.replaces
                ));
            }
            if entry.clear_freeze_blocker {
                let case = negatives["cases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["id"] == entry.case.as_str())
                    .unwrap();
                if case["freeze_blocker"] != true {
                    return Err(format!(
                        "{} has no freeze_blocker flag to clear",
                        entry.case
                    ));
                }
            }
        }
        Ok(())
    }

    /// Why a frozen case is not executable, after this erratum set: a cleared
    /// freeze_blocker flag and an `executes` entry lift the frozen reason.
    pub fn not_executable<'a>(&self, case: &'a Value) -> Option<&'a str> {
        let id = case["id"].as_str().unwrap_or_default();
        let entry = self.entries.iter().find(|e| {
            e.source == "negative-cases.json"
                && e.case == id
                && (e.clear_freeze_blocker || e.executes.is_some())
        });
        match entry {
            Some(_) => None,
            None => super::not_executable(case),
        }
    }

    /// Every override must have been observed.
    #[cfg(test)]
    pub fn ensure_all_observed(
        &self,
        seen: &BTreeSet<(String, String, String, String)>,
    ) -> Result<(), String> {
        for entry in &self.entries {
            for key in entry.observations.keys() {
                let id = (
                    entry.source.clone(),
                    entry.case.clone(),
                    entry.variant.clone(),
                    key.clone(),
                );
                if !seen.contains(&id) {
                    return Err(format!("erratum override not observed: {id:?}"));
                }
            }
        }
        Ok(())
    }
}

/// Compare one observation with its override and report it as ERRATUM, with
/// the replaced and the new expectation. Returns whether it agrees.
#[cfg(test)]
pub fn report(
    path: &str,
    entry: &ErratumEntry,
    override_: &ErratumObservation,
    observation: &Observation,
) -> bool {
    let agrees = observation.outcome == override_.outcome;
    println!(
        "ERRATUM {} {path} [{}]: replaces {}; expected {} ({}); observed {}; {}; detail={:?}",
        if agrees { "PASS" } else { "DISAGREEMENT" },
        observation.key,
        entry.replaces,
        override_.outcome,
        override_.author_observation,
        observation.outcome,
        observation.location,
        observation.detail
    );
    if let Some(open) = override_.expected.get("open") {
        println!(
            "INFORMATIVE {path} [{}] erratum open: {open}",
            observation.key
        );
    }
    for quote in override_.expected["quotes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let quote = quote.as_str().expect("quote text");
        assert!(
            crate::tape_image_vectors::specification_quote_holds(quote),
            "{path} [{}]: erratum quote not in the specification: {quote}",
            observation.key
        );
    }
    agrees
}
