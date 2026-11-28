//! (El1xr) `VICTORY_RAW_EXCEPT=<part>,..` (#96): the indirect executions of
//! title pipelines whose shaders' hashes or names contain one of the parts
//! are not run raw from the title's record (`VICTORY_INDIRECT_RAW=2`) but
//! bounded by our pass just before, as before raw. For the one-frame dark
//! environment: the title's depth of field sizes its blur passes
//! (`CS_MainBlur`, `CS_Composite`, ..) by records its tile classification
//! writes in the same frame; read raw, a record not yet written (or the
//! reset's zeros) would blur nothing, and the out-of-focus walls, window and
//! people come out black for that frame while the subject in focus stays
//! lit. `DOF` stands for the title's depth-of-field passes. Off (unset):
//! nothing changes.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The parts, `DOF` expanded; empty when the switch is off.
fn parts() -> &'static [String] {
    static PARTS: OnceLock<Vec<String>> = OnceLock::new();
    PARTS.get_or_init(|| parse(&std::env::var("VICTORY_RAW_EXCEPT").unwrap_or_default()))
}

/// The depth-of-field passes `DOF` names: those that run from records.
const DOF: [&str; 7] = [
    "CS_TileClassification",
    "CS_PrefilterColor",
    "CS_PrefilterBoundaryColor",
    "CS_SkipPrefilterColor_",
    "CS_MainBlur_",
    "CS_SimpleMainBlur_",
    "CS_Composite_",
];

fn parse(value: &str) -> Vec<String> {
    let mut v = Vec::new();
    for part in value.split(',').map(str::trim).filter(|p| !p.is_empty() && *p != "0") {
        if part.eq_ignore_ascii_case("dof") {
            v.extend(DOF.iter().map(|&n| n.to_owned()));
        } else {
            v.push(part.to_owned());
        }
    }
    v
}

/// Whether `names` (a pipeline's shaders as `crate::pipeline::named_shaders_of`
/// writes them) contains one of `parts`.
fn matches(names: &str, parts: &[String]) -> bool {
    parts.iter().any(|p| names.contains(p.as_str()))
}

/// Whether title pipeline `this` is kept off the raw path; looked up once per
/// pipeline.
#[cfg(windows)]
pub(crate) fn excepted(this: usize) -> bool {
    let parts = parts();
    if parts.is_empty() {
        return false;
    }
    static SEEN: OnceLock<Mutex<HashMap<usize, bool>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Mutex::default);
    if let Some(&e) = seen.lock().unwrap_or_else(|e| e.into_inner()).get(&this) {
        return e;
    }
    let names = crate::pipeline::named_shaders_of(this as crate::object::This);
    let e = matches(&names, parts);
    if e {
        tracing::info!("VICTORY_RAW_EXCEPT: bounded, not raw: {names}");
    }
    seen.lock().unwrap_or_else(|e| e.into_inner()).insert(this, e);
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dof_expands_and_parts_match_hashes_or_names() {
        assert!(parse("").is_empty());
        assert!(parse("0").is_empty());
        let p = parse("dof, e30970f1727063fd");
        assert_eq!(p.len(), DOF.len() + 1);
        assert!(matches("[compute 03c01507703d7449 CS_MainBlur_446b3af2_Wrapped]", &p));
        assert!(matches("[compute e30970f1727063fd]", &p));
        assert!(!matches("[compute 6a04b371a8e26853 CS_ZeroOperation_63bfc116_Wrapped]", &p));
        // The reset is no reader of records.
        assert!(!matches("[compute e080e6a7cfb63a5b CS_ResetIndirectArgsBuffer_6615455a_Wrapped]", &p));
    }
}
