//! Static provider declarations and version-scoped live observations.
//!
//! A declaration says the agent contains a path for a capability. It is not
//! proof that a local provider version, hook or plugin is ready. Live evidence
//! is deliberately exact-version and remains separate from the declarations.

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Declarations<'a> {
    pub detection: &'a str,
    pub prompt: &'a str,
    pub interrupt: &'a str,
    pub resume: bool,
    pub answers: bool,
    pub integration: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimeProbe<'a> {
    pub found: bool,
    pub versions_requested: bool,
    pub version_output: Option<&'a str>,
    pub version_error: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct LiveEvidence {
    observed_on: &'static str,
    source: &'static str,
    source_section: &'static str,
    versions: &'static [&'static str],
    capabilities: &'static [&'static str],
    limitations: &'static [&'static str],
}

const PUBLIC_EVIDENCE: &str = "docs/public/provider-observations.md";

const CLAUDE: &[LiveEvidence] = &[LiveEvidence {
    observed_on: "2026-11-17",
    source: PUBLIC_EVIDENCE,
    source_section: "#claude",
    versions: &["2.1.326"],
    capabilities: &["detection", "prompt", "interrupt", "resume"],
    limitations: &[
        "prompt acceptance and lifecycle required the exported hook",
        "the provider visibly interrupted, but the interrupt receipt itself was unconfirmed",
    ],
}];
const CODEX: &[LiveEvidence] = &[LiveEvidence {
    observed_on: "2026-11-17",
    source: PUBLIC_EVIDENCE,
    source_section: "#codex",
    versions: &["0.169.0"],
    capabilities: &["detection", "prompt", "interrupt"],
    limitations: &["prompt delivery did not prove provider acceptance"],
}];
const OPENCODE: &[LiveEvidence] = &[
    LiveEvidence {
        observed_on: "2026-11-17",
        source: PUBLIC_EVIDENCE,
        source_section: "#opencode",
        versions: &["1.19.5"],
        capabilities: &["detection", "prompt", "interrupt"],
        limitations: &["the run used --answers, but no approval response was exercised"],
    },
    LiveEvidence {
        observed_on: "2026-11-17",
        source: PUBLIC_EVIDENCE,
        source_section: "#opencode",
        versions: &["1.19.6"],
        capabilities: &["detection"],
        limitations: &["only 80-column and 150-column working-screen detection was checked"],
    },
    LiveEvidence {
        observed_on: "2026-10-26",
        source: PUBLIC_EVIDENCE,
        source_section: "#opencode",
        versions: &["1.19.4"],
        capabilities: &["answers"],
        limitations: &["answers requires a run started with --answers"],
    },
];
const KILO: &[LiveEvidence] = &[
    LiveEvidence {
        observed_on: "2026-11-17",
        source: PUBLIC_EVIDENCE,
        source_section: "#kilo",
        versions: &["7.8.17"],
        capabilities: &["detection", "prompt", "interrupt"],
        limitations: &["prompt delivery had no provider acceptance digest"],
    },
    LiveEvidence {
        observed_on: "2026-10-26",
        source: PUBLIC_EVIDENCE,
        source_section: "#kilo",
        versions: &["7.8.17"],
        capabilities: &["answers"],
        limitations: &["answers requires a run started with --answers"],
    },
];
const OMP: &[LiveEvidence] = &[LiveEvidence {
    observed_on: "2026-11-17",
    source: PUBLIC_EVIDENCE,
    source_section: "#omp",
    versions: &["18.12.2"],
    capabilities: &["detection", "prompt", "interrupt", "resume"],
    limitations: &["lifecycle evidence required the exported extension"],
}];
const PI: &[LiveEvidence] = &[
    LiveEvidence {
        observed_on: "2026-10-04",
        source: PUBLIC_EVIDENCE,
        source_section: "#pi",
        versions: &["0.82.1"],
        capabilities: &["detection", "resume"],
        limitations: &["prompt could not be checked because authentication failed"],
    },
    LiveEvidence {
        observed_on: "2026-11-17",
        source: PUBLIC_EVIDENCE,
        source_section: "#pi",
        versions: &["0.84.2"],
        capabilities: &["detection"],
        limitations: &["model execution could not be checked because authentication failed"],
    },
];
const CURSOR: &[LiveEvidence] = &[LiveEvidence {
    observed_on: "2026-11-17",
    source: PUBLIC_EVIDENCE,
    source_section: "#cursor",
    versions: &["2026.10.22"],
    capabilities: &["detection"],
    limitations: &["the provider was logged out, so prompt and interrupt were not rechecked"],
}];
const AGY: &[LiveEvidence] = &[LiveEvidence {
    observed_on: "2026-10-26",
    source: PUBLIC_EVIDENCE,
    source_section: "#agy",
    versions: &["1.2.16"],
    capabilities: &["detection"],
    limitations: &["session resume requires a user-installed hook"],
}];
const KIMI: &[LiveEvidence] = &[LiveEvidence {
    observed_on: "2026-10-04",
    source: PUBLIC_EVIDENCE,
    source_section: "#kimi",
    versions: &["2.1.1"],
    capabilities: &["detection"],
    limitations: &["prompt and interrupt could not be checked because authentication failed"],
}];
const GEMINI: &[LiveEvidence] = &[LiveEvidence {
    observed_on: "2026-11-17",
    source: PUBLIC_EVIDENCE,
    source_section: "#gemini",
    versions: &["0.46.0"],
    capabilities: &["detection"],
    limitations: &["login failed, so prompt, interrupt and resume were not checked"],
}];
const MASTRACODE: &[LiveEvidence] = &[LiveEvidence {
    observed_on: "2026-11-17",
    source: PUBLIC_EVIDENCE,
    source_section: "#mastracode",
    versions: &["0.50.0"],
    capabilities: &[],
    limitations: &["only the setup wizard was observed"],
}];
const NONE: &[LiveEvidence] = &[];

fn evidence(provider: &str) -> &'static [LiveEvidence] {
    match provider {
        "claude" => CLAUDE,
        "codex" => CODEX,
        "opencode" => OPENCODE,
        "kilo" => KILO,
        "omp" => OMP,
        "pi" => PI,
        "cursor" => CURSOR,
        "agy" => AGY,
        "kimi" => KIMI,
        "gemini" => GEMINI,
        "mastracode" => MASTRACODE,
        _ => NONE,
    }
}

fn known_limitations(provider: &str) -> &'static [&'static str] {
    match provider {
        "claude" => &["without the exported hook, streamed answers can look idle"],
        "codex" => &["prompt delivery is PTY delivery, not provider acceptance"],
        "opencode" | "kilo" => &[
            "approval and question responses require a run started with --answers",
            "the answer channel is checked per run and is not implied by installation",
        ],
        "cursor" => &[
            "prompt uses a guarded two-step paste",
            "slash-prefixed prompts are refused because they open the command menu",
        ],
        "omp" | "pi" => &[
            "without the exported extension, an idle screen may remain unknown",
            "export availability does not mean the extension is installed",
        ],
        "gemini" | "kimi" => &["authenticated prompt and interrupt behavior is not live-tested"],
        "mastracode" => &["the observed setup wizard has no detection rule"],
        _ if evidence(provider).is_empty() => {
            &["no versioned live provider run is recorded; declarations are not verification"]
        }
        _ => &["live evidence applies only to the exact versions and dates listed"],
    }
}

/// Additive support data for provider listings, capability reports and doctor.
pub(crate) fn report(
    provider: &str,
    declarations: Declarations<'_>,
    runtime: Option<RuntimeProbe<'_>>,
) -> Value {
    let evidence = evidence(provider);
    let parsed_version = runtime
        .and_then(|probe| probe.version_output)
        .and_then(extract_version);
    let matched = parsed_version
        .map(|version| {
            evidence
                .iter()
                .filter(|entry| entry.versions.contains(&version))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let version_evidence = match runtime {
        None => "not_checked",
        Some(probe) if !probe.found => "provider_not_found",
        Some(probe) if !probe.versions_requested => "not_requested",
        Some(probe) if probe.version_error.is_some() => "probe_failed",
        Some(_) if parsed_version.is_none() => "unparsed",
        Some(_) if !matched.is_empty() => "matched",
        Some(_) => "not_observed",
    };
    let verification = json!({
        "status": "unverified",
        "version_evidence": version_evidence,
        "parsed_version": parsed_version,
        "matched_observations": matched.iter().map(|entry| json!({
            "version": parsed_version,
            "observed_on": entry.observed_on,
            "source": entry.source,
            "source_section": entry.source_section,
            "capabilities": entry.capabilities,
        })).collect::<Vec<_>>(),
        "integration_installation_verified": false,
    });
    let live_evidence = evidence
        .iter()
        .map(|entry| {
            json!({
                "observed_on": entry.observed_on,
                "source": entry.source,
                "source_section": entry.source_section,
                "versions": entry.versions,
                "capabilities": entry.capabilities,
                "limitations": entry.limitations,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema": 1,
        "declarations": {
            "detection": declarations.detection,
            "prompt": declarations.prompt,
            "interrupt": declarations.interrupt,
            "resume": declarations.resume,
            "answers": declarations.answers,
            "integration": declarations.integration.unwrap_or("none"),
        },
        "verification": verification,
        "live_evidence": live_evidence,
        "known_limitations": known_limitations(provider),
        "integration_readiness": integration_readiness(provider, declarations.integration),
    })
}

fn integration_readiness(provider: &str, capability: Option<&str>) -> Value {
    let Some(capability) = capability else {
        return json!({
            "status": "not_available",
            "installation": "not_applicable",
            "export_available": false,
            "guidance": ["Use the declared screen/process evidence and inspect capabilities before automation."],
        });
    };
    json!({
        "status": "unverified",
        "installation": "unverified",
        "capability": capability,
        "export_available": true,
        "guidance": [
            format!("Inside a masil pane, run `masil-agent agent integration export {provider} --directory ABSOLUTE_PATH`. From another shell, pass `--socket PATH` before `integration`."),
            "Review the returned files and registration instructions, install them in the provider, then start a new managed run.",
            "Use run capabilities/callback evidence to confirm readiness; export support alone does not prove installation.",
        ],
    })
}

fn extract_version(output: &str) -> Option<&str> {
    output
        .split_whitespace()
        .map(|candidate| candidate.trim_matches(['(', ')', '[', ']', '{', '}', ',', ';']))
        .find(|candidate| {
            candidate.split('.').count() >= 3
                && candidate
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declarations(integration: Option<&'static str>) -> Declarations<'static> {
        Declarations {
            detection: "screen_manifest",
            prompt: "guarded_paste",
            interrupt: "measured_keys",
            resume: true,
            answers: false,
            integration,
        }
    }

    #[test]
    fn a_different_runtime_version_never_becomes_verified() {
        let report = report(
            "claude",
            declarations(Some("native_session_only")),
            Some(RuntimeProbe {
                found: true,
                versions_requested: true,
                version_output: Some("2.1.327 (Claude Code)"),
                version_error: None,
            }),
        );
        assert_eq!(report["verification"]["status"], "unverified");
        assert_eq!(report["verification"]["version_evidence"], "not_observed");
        assert!(
            report["verification"]["matched_observations"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            report["verification"]["integration_installation_verified"],
            false
        );
    }

    #[test]
    fn an_exact_version_match_is_evidence_not_a_readiness_claim() {
        let report = report(
            "claude",
            declarations(Some("native_session_only")),
            Some(RuntimeProbe {
                found: true,
                versions_requested: true,
                version_output: Some("claude 2.1.326"),
                version_error: None,
            }),
        );
        assert_eq!(report["verification"]["status"], "unverified");
        assert_eq!(report["verification"]["version_evidence"], "matched");
        assert_eq!(
            report["verification"]["matched_observations"][0]["observed_on"],
            "2026-11-17"
        );
        assert_eq!(
            report["integration_readiness"]["installation"],
            "unverified"
        );
    }

    #[test]
    fn a_prerelease_version_does_not_match_stable_evidence() {
        let report = report(
            "codex",
            declarations(Some("native_session_only")),
            Some(RuntimeProbe {
                found: true,
                versions_requested: true,
                version_output: Some("codex-cli 0.169.0-beta.1"),
                version_error: None,
            }),
        );
        assert_eq!(report["verification"]["status"], "unverified");
        assert_ne!(report["verification"]["version_evidence"], "matched");
        assert!(
            report["verification"]["matched_observations"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn answers_evidence_is_not_inferred_from_an_answers_mode_prompt() {
        let probe = |version| RuntimeProbe {
            found: true,
            versions_requested: true,
            version_output: Some(version),
            version_error: None,
        };
        let prompt_run = report(
            "opencode",
            declarations(Some("full_lifecycle")),
            Some(probe("opencode 1.19.5")),
        );
        let answer_run = report(
            "opencode",
            declarations(Some("full_lifecycle")),
            Some(probe("opencode 1.19.4")),
        );
        assert!(
            !prompt_run["verification"]["matched_observations"][0]["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("answers"))
        );
        assert!(
            answer_run["verification"]["matched_observations"][0]["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("answers"))
        );
    }

    #[test]
    fn evidence_provenance_names_only_public_repository_documents() {
        for provider in [
            "claude",
            "codex",
            "opencode",
            "kilo",
            "omp",
            "pi",
            "cursor",
            "agy",
            "kimi",
            "gemini",
            "mastracode",
        ] {
            for entry in evidence(provider) {
                assert!(entry.source.starts_with("docs/"), "{}", entry.source);
                assert!(!entry.source.contains("/tmp") && !entry.source.contains(".build"));
                assert!(!entry.source_section.is_empty());
            }
        }
    }

    #[test]
    fn an_exportable_integration_always_has_actionable_guidance() {
        let report = report("omp", declarations(Some("full_lifecycle")), None);
        let guidance = report["integration_readiness"]["guidance"]
            .as_array()
            .unwrap();
        assert!(!guidance.is_empty());
        assert!(
            guidance[0]
                .as_str()
                .unwrap()
                .contains("integration export omp")
        );
        assert_eq!(report["verification"]["status"], "unverified");
    }
}
