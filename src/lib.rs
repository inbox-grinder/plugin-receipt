//! Conservative labelled English receipts. Missing or ambiguous identity/total stays ordinary mail.
use chrono::{DateTime, NaiveDate, SecondsFormat};
use plugin_sdk::{
    EmailInput, FieldCandidate, RecordContribution, RecordIdentity, RecordResult, SemanticUpdate,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

plugin_sdk::export_plugin!(extract);

pub fn extract(input: EmailInput) -> RecordResult {
    let mut result = RecordResult {
        refresh_requests: Vec::new(),
        schema_version: "1".into(),
        sources: input
            .invocation
            .as_ref()
            .map(|i| i.sources.clone())
            .unwrap_or_default(),
        records: vec![],
    };
    let Some(source) = input
        .message
        .source_id
        .as_ref()
        .filter(|id| result.sources.iter().any(|s| &s.id == *id))
    else {
        return result;
    };
    let Some(date) = input.date.as_deref().and_then(|s| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .or_else(|| DateTime::parse_from_rfc2822(s).ok())
    }) else {
        return result;
    };
    let observed = date.to_utc().to_rfc3339_opts(SecondsFormat::Secs, true);
    let domain = input.sender.domain.trim().to_ascii_lowercase();
    if domain.is_empty() {
        return result;
    }
    let text = input
        .text_body
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| html_text(input.html_body.as_deref().unwrap_or("")));
    let lines: Vec<String> = text
        .lines()
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
        .collect();
    let starts: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, s)| order(s).map(|_| i))
        .collect();
    let mut identities = BTreeSet::new();
    for (index, start) in starts.iter().enumerate() {
        let block = &lines[*start..starts.get(index + 1).copied().unwrap_or(lines.len())];
        let number = order(&block[0]).unwrap().trim_start_matches('#').trim();
        if number.is_empty()
            || number.len() > 100
            || !number
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            continue;
        }
        let totals: BTreeSet<_> = block
            .iter()
            .filter_map(|s| labelled(s, &["grand total:", "total:", "amount paid:"]))
            .collect();
        if totals.len() != 1 {
            continue;
        }
        let Some(total) = money(totals.first().unwrap()) else {
            continue;
        };
        let key = hash(&json!([domain, number.to_ascii_lowercase()]));
        // Repeated/conflicting blocks for one order cannot be safely assigned evidence.
        if !identities.insert(key.clone()) {
            result.records.retain(|r| r.identity.key != key);
            continue;
        }
        let merchant = unique_label(&lines[..*start], &["merchant:"]).unwrap_or(&domain);
        let mut values = BTreeMap::from([
            ("merchant".to_owned(), json!(merchant)),
            ("orderNumber".to_owned(), json!(number)),
            ("total".to_owned(), total),
        ]);
        if let Some(day) = unique_label(block, &["order date:", "receipt date:", "date:"]) {
            if NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok() {
                values.insert("purchaseDate".into(), json!(day));
            }
        }
        let items: BTreeSet<_> = block
            .iter()
            .filter_map(|s| labelled(s, &["item:"]))
            .filter(|s| !s.is_empty())
            .collect();
        if !items.is_empty() {
            values.insert("items".into(), json!(items));
        }
        // Delivery timestamp, message ID and source ID do not change semantic identity.
        let update_key = format!("receipt:{}", hash(&json!(values)));
        let occurred = input
            .invocation
            .as_ref()
            .and_then(|i| {
                i.records.iter().find(|r| {
                    r["identity"]["namespace"] == "receipt" && r["identity"]["key"] == key
                })
            })
            .and_then(|r| r["updates"][&update_key]["occurredAt"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                values
                    .get("purchaseDate")
                    .and_then(Value::as_str)
                    .map(|s| format!("{s}T00:00:00Z"))
            })
            .unwrap_or_else(|| observed.clone());
        let fields: BTreeMap<_, _> = values
            .into_iter()
            .map(|(name, value)| {
                (
                    name,
                    FieldCandidate {
                        value,
                        evidence: vec![source.clone()],
                        basis: "extracted".into(),
                        confidence: 1.0,
                        observed_at: observed.clone(),
                        revision: None,
                    },
                )
            })
            .collect();
        result.records.push(RecordContribution {
            identity: RecordIdentity { namespace: "receipt".into(), key }, record_type: "receipt".into(),
            updates: vec![SemanticUpdate { key: update_key, occurred_at: occurred, summary: format!("Receipt for order {number}"), notify: true, evidence: vec![source.clone()], fields: fields.keys().cloned().collect(), supersedes: None }],
            fields, attention: vec![], links: vec![],
            presentation: json!({"family":"receipt","version":1,"title":format!("{merchant} · {number}")}),
            // Retention is a host/user decision. Do not start automatic record archiving.
            lifecycle: json!({"state":"active"}),
        });
    }
    result
}

fn hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}
fn labelled<'a>(s: &'a str, labels: &[&str]) -> Option<&'a str> {
    let lower = s.to_ascii_lowercase();
    labels
        .iter()
        .find_map(|label| lower.starts_with(label).then(|| s[label.len()..].trim()))
}
fn unique_label<'a>(lines: &'a [String], labels: &[&str]) -> Option<&'a str> {
    let values: BTreeSet<_> = lines.iter().filter_map(|s| labelled(s, labels)).collect();
    (values.len() == 1).then(|| *values.first().unwrap())
}
fn order(s: &str) -> Option<&str> {
    labelled(s, &["order number:", "order id:", "order #:", "order #"])
}
fn money(s: &str) -> Option<Value> {
    let parts: Vec<_> = s.split_whitespace().collect();
    if parts.len() != 2 {
        return None;
    }
    let (currency, amount) =
        if parts[0].len() == 3 && parts[0].bytes().all(|b| b.is_ascii_uppercase()) {
            (parts[0], parts[1])
        } else {
            (parts[1], parts[0])
        };
    if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let (whole, fraction) = amount.split_once('.')?;
    if whole.is_empty()
        || fraction.len() != 2
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    Some(
        json!({"amount":format!("{}.{}", if whole.is_empty() { "0" } else { whole }, fraction),"currency":currency}),
    )
}
fn html_text(html: &str) -> String {
    let mut out = String::new();
    let mut tag = String::new();
    let mut inside = false;
    let mut hidden = false;
    for c in html.chars() {
        match c {
            '<' => {
                inside = true;
                tag.clear();
            }
            '>' if inside => {
                inside = false;
                let name = tag
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ["script", "style"].contains(&name.as_str()) {
                    hidden = true
                }
                if ["/script", "/style"].contains(&name.as_str()) {
                    hidden = false
                }
                if !hidden
                    && ["br", "br/", "/p", "/div", "/tr", "/li", "/h1", "/h2"]
                        .contains(&name.as_str())
                {
                    out.push('\n')
                }
                if !hidden && ["/td", "/th"].contains(&name.as_str()) {
                    out.push(' ')
                }
            }
            _ if inside => tag.push(c),
            _ if !hidden => out.push(c),
            _ => {}
        }
    }
    html_escape::decode_html_entities(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input() -> EmailInput {
        serde_json::from_str(include_str!("../fixtures/email-input.json")).unwrap()
    }
    #[test]
    fn extracts_text_and_html_with_evidence() {
        let a = extract(input());
        let b = extract(serde_json::from_str(include_str!("../fixtures/html-input.json")).unwrap());
        assert_eq!(a.records.len(), 1);
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap()
        );
        assert_eq!(
            a.records[0].fields["total"].value,
            json!({"amount":"42.00","currency":"USD"})
        );
        assert_eq!(
            a.records[0].fields["items"].value,
            json!(["2 × Notebook @ USD 21.00"])
        );
        for field in a.records[0].fields.values() {
            assert_eq!(field.evidence, vec!["source-1"]);
        }
    }
    #[test]
    fn duplicate_delivery_and_changed_total() {
        let first = extract(input());
        let mut duplicate = input();
        duplicate.date = Some("2026-10-26T12:00:00Z".into());
        duplicate.message.uid += 1;
        let second = extract(duplicate);
        assert_eq!(first.records[0].identity, second.records[0].identity);
        assert_eq!(
            first.records[0].updates[0].key,
            second.records[0].updates[0].key
        );
        let mut changed = input();
        changed.text_body = changed.text_body.map(|s| s.replace("42.00", "43.00"));
        assert_ne!(
            first.records[0].updates[0].key,
            extract(changed).records[0].updates[0].key
        );
    }
    #[test]
    fn rejects_ambiguous_or_incomplete_and_supports_multiple_orders() {
        for body in [
            "Order number: A\nTotal: $42.00",
            "Total: USD 42.00",
            "Order number: A\nTotal: USD 42.00\nTotal: USD 43.00",
            "Order number: A\nTotal: USD 42.00\nOrder number: A\nTotal: USD 43.00",
        ] {
            let mut i = input();
            i.text_body = Some(body.into());
            assert!(extract(i).records.is_empty());
        }
        let mut i = input();
        i.text_body =
            Some("Order number: A\nTotal: USD 1.00\nOrder number: B\nTotal: EUR 2.00".into());
        assert_eq!(extract(i).records.len(), 2);
        let mut i = input();
        i.message.source_id = None;
        assert!(extract(i).records.is_empty());
    }
}
