//! What an MCP server asks the user (an MCP "elicitation"), as the app's
//! own question card shows it: each field of the server's form becomes a
//! question (a choice becomes its options, yes or no two options, text a
//! line to type in), and the answers go back as the values the form asked
//! for. Nothing here trusts the server: what is not shaped like a field is
//! left out, and a form is kept small.

use serde_json::{Map, Value, json};

/// Most fields of a form that are asked, and most options of one field.
const MOST: usize = 8;
/// Longest text of the server's that is shown, in characters.
const LONGEST: usize = 600;

/// What one field takes.
#[derive(Debug, Clone, PartialEq)]
enum Kind {
    /// Text of the user's own.
    Text,
    /// A number; whole when `integer`.
    Number {
        /// Whether only whole numbers are taken.
        integer: bool,
    },
    /// Yes or no.
    Flag,
    /// One of these values, each with its label.
    Choice(Vec<(Value, String)>),
    /// Any of these values, each with its label.
    Many(Vec<(Value, String)>),
}

/// One field of a server's form.
#[derive(Debug, Clone, PartialEq)]
struct Field {
    /// Its name in the form.
    name: String,
    /// The question it is asked as: the card files its answer under these words.
    question: String,
    /// What it takes.
    kind: Kind,
}

/// A server's form: its fields, in order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Form {
    fields: Vec<Field>,
}

/// At most [`LONGEST`] characters of `text`, trimmed.
fn short(text: &str) -> String {
    text.trim().chars().take(LONGEST).collect()
}

/// The string at `key` of `value`, trimmed; empty when there is none.
fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).map(short).unwrap_or_default()
}

/// The choices a field offers, as `(value, label)`: `enum` (labelled by
/// `enumNames` where given), or `oneOf`/`anyOf` of `const`s with titles.
fn choices(field: &Value) -> Option<Vec<(Value, String)>> {
    let label = |value: &Value| match value {
        Value::String(text) => short(text),
        other => other.to_string(),
    };
    if let Some(values) = field.get("enum").and_then(Value::as_array) {
        let names = field.get("enumNames").and_then(Value::as_array);
        let pair = |index: usize, value: &Value| {
            let name = names.and_then(|names| names.get(index)).and_then(Value::as_str).map(short);
            (value.clone(), name.filter(|name| !name.is_empty()).unwrap_or_else(|| label(value)))
        };
        return Some(values.iter().take(MOST).enumerate().map(|(index, value)| pair(index, value)).collect());
    }
    let listed = field.get("oneOf").or_else(|| field.get("anyOf")).and_then(Value::as_array)?;
    let picked = listed.iter().filter_map(|option| {
        let value = option.get("const")?;
        let title = text(option, "title");
        Some((value.clone(), if title.is_empty() { label(value) } else { title }))
    });
    let picked: Vec<_> = picked.take(MOST).collect();
    (!picked.is_empty()).then_some(picked)
}

/// What the field `schema` takes; `None` for a kind of field the card cannot ask.
fn kind(schema: &Value) -> Option<Kind> {
    let of = schema.get("type").and_then(Value::as_str).unwrap_or("string");
    Some(match of {
        "array" => Kind::Many(choices(schema.get("items")?)?),
        _ if choices(schema).is_some() => Kind::Choice(choices(schema)?),
        "string" => Kind::Text,
        "number" => Kind::Number { integer: false },
        "integer" => Kind::Number { integer: true },
        "boolean" => Kind::Flag,
        _ => return None,
    })
}

impl Form {
    /// The form a server asks for with `message`, from its JSON Schema
    /// `schema` (an object of `properties`) and the fields' order where it
    /// said it. `None` when it has no field the card can ask: nothing but
    /// a go-ahead is wanted then.
    pub(crate) fn new(message: &str, schema: &Value, order: &[String]) -> Option<Self> {
        let properties = schema.get("properties").and_then(Value::as_object)?;
        // The order the server listed them in, and then whatever it did not list.
        let mut names: Vec<&String> = order.iter().filter(|name| properties.contains_key(*name)).collect();
        names.extend(properties.keys().filter(|name| !order.contains(name)));
        let mut fields: Vec<Field> = Vec::new();
        for name in names.into_iter().take(MOST) {
            let field = &properties[name];
            let Some(kind) = kind(field) else { continue };
            let title = text(field, "title");
            let about = text(field, "description");
            let mut question = match (title.is_empty(), about.is_empty()) {
                (false, false) => format!("{title}: {about}"),
                (false, true) => title,
                (true, false) => about,
                (true, true) => name.clone(),
            };
            // The card files answers under the questions' words: no two may be the same.
            if fields.iter().any(|known| known.question == question) {
                question = format!("{question} ({name})");
            }
            fields.push(Field { name: name.clone(), question, kind });
        }
        // A form of one field is asked with the server's own words.
        let message = short(message);
        if let [only] = fields.as_mut_slice()
            && !message.is_empty()
            && only.question != message
        {
            only.question = format!("{message}\n{}", only.question);
        }
        (!fields.is_empty()).then_some(Self { fields })
    }

    /// The input of the question card that asks the form: questions in the
    /// shape of Claude's own question tool ([`crate::engine::QUESTION_TOOL`]).
    pub(crate) fn questions(&self) -> String {
        let options = |values: &[(Value, String)]| -> Vec<Value> {
            values.iter().map(|(_, label)| json!({ "label": label, "description": "" })).collect()
        };
        let asked: Vec<Value> = self
            .fields
            .iter()
            .map(|field| {
                let (listed, multi) = match &field.kind {
                    Kind::Text | Kind::Number { .. } => (Vec::new(), false),
                    Kind::Flag => (vec![json!({ "label": "Yes" }), json!({ "label": "No" })], false),
                    Kind::Choice(values) => (options(values), false),
                    Kind::Many(values) => (options(values), true),
                };
                let header: String = field.name.chars().take(24).collect();
                json!({ "question": field.question, "header": header, "options": listed, "multiSelect": multi })
            })
            .collect();
        json!({ "questions": asked }).to_string()
    }

    /// The values to send the server for the card's answers (`answered`,
    /// the card's input with its `answers` by question): each field's
    /// answer as the form wants it. A field without an answer is left out.
    pub(crate) fn content(&self, answered: &str) -> Value {
        let answered: Value = serde_json::from_str(answered).unwrap_or(Value::Null);
        let answers = answered.get("answers");
        let mut content = Map::new();
        for field in &self.fields {
            let Some(answer) = answers.and_then(|answers| answers.get(&field.question)).and_then(Value::as_str) else {
                continue;
            };
            let answer = answer.trim();
            let value_of = |values: &[(Value, String)], said: &str| {
                values.iter().find(|(_, label)| label == said).map_or_else(|| said.into(), |(value, _)| value.clone())
            };
            let value = match &field.kind {
                Kind::Text => answer.into(),
                Kind::Number { integer: true } => answer.parse::<i64>().map_or_else(|_| answer.into(), Value::from),
                Kind::Number { integer: false } => match answer.parse::<f64>() {
                    Ok(number) if number.is_finite() => json!(number),
                    _ => answer.into(),
                },
                Kind::Flag => matches!(answer.to_lowercase().as_str(), "yes" | "y" | "true" | "1" | "on").into(),
                Kind::Choice(values) => value_of(values, answer),
                // The card joins what was picked with commas; the labels are matched whole first.
                Kind::Many(values) => {
                    let mut picked = Vec::new();
                    let mut rest = answer;
                    while !rest.is_empty() {
                        let known = values.iter().find(|(_, label)| {
                            rest.strip_prefix(label.as_str())
                                .is_some_and(|after| after.is_empty() || after.starts_with(", "))
                        });
                        let (value, taken) = if let Some((value, label)) = known {
                            (value.clone(), label.len())
                        } else {
                            let end = rest.find(", ").unwrap_or(rest.len());
                            (rest[..end].into(), end)
                        };
                        picked.push(value);
                        rest = rest[taken..].trim_start_matches(", ");
                    }
                    Value::Array(picked)
                }
            };
            content.insert(field.name.clone(), value);
        }
        Value::Object(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card's answer to `form`: `answers` filed under each question.
    fn answer(form: &Form, answers: &[&str]) -> String {
        let mut input: Value = serde_json::from_str(&form.questions()).unwrap();
        let filed: Map<String, Value> =
            form.fields.iter().zip(answers).map(|(field, said)| (field.question.clone(), (*said).into())).collect();
        input["answers"] = Value::Object(filed);
        input.to_string()
    }

    #[test]
    fn a_form_is_asked_field_by_field_and_answered_as_it_asks() {
        let schema = json!({ "type": "object", "properties": {
            "count": { "type": "integer", "title": "How many" },
            "region": { "type": "string", "enum": ["eu-west", "us-east"], "enumNames": ["Europe", "America"] },
            "notify": { "type": "boolean", "description": "Send a mail when done" },
            "tags": { "type": "array", "items": { "anyOf": [{ "const": "a", "title": "Alpha" }, { "const": "b, c" }] } },
            "name": { "type": "string" },
            "nested": { "type": "object" },
        }, "required": ["count"] });
        let order = ["region".to_owned(), "count".to_owned(), "gone".to_owned()];
        let form = Form::new("Set up the job", &schema, &order).expect("fields");
        let names: Vec<&str> = form.fields.iter().map(|field| field.name.as_str()).collect();
        assert_eq!(names, ["region", "count", "name", "notify", "tags"], "the server's order, then the rest by name");
        assert!(!names.contains(&"nested"), "what the card cannot ask is left out");

        let input: Value = serde_json::from_str(&form.questions()).unwrap();
        let region = &input["questions"][0];
        assert_eq!(
            region["options"],
            json!([{ "label": "Europe", "description": "" }, { "label": "America", "description": "" }])
        );
        let notify = input["questions"].as_array().unwrap().iter().find(|q| q["header"] == "notify").unwrap();
        assert_eq!(notify["question"], "Send a mail when done");
        assert_eq!(notify["options"].as_array().map(Vec::len), Some(2));

        let sent = form.content(&answer(&form, &["America", "12", "Ada", "Yes", "Alpha, b, c, other"]));
        assert_eq!(
            sent,
            json!({ "region": "us-east", "count": 12, "notify": true, "tags": ["a", "b, c", "other"], "name": "Ada" })
        );
        let typed = form.content(&answer(&form, &["Mars", "twelve", "Bea", "no"]));
        assert_eq!(
            typed,
            json!({ "region": "Mars", "count": "twelve", "name": "Bea", "notify": false }),
            "words of the user's own go as they are"
        );
    }

    #[test]
    fn one_field_is_asked_in_the_servers_words_and_no_field_is_no_form() {
        let schema = json!({ "properties": { "repo": { "type": "string", "title": "Repository" } } });
        let form = Form::new("Which repository should I watch?", &schema, &[]).unwrap();
        assert_eq!(form.fields[0].question, "Which repository should I watch?\nRepository");
        for nothing in
            [json!(null), json!({ "properties": {} }), json!({ "properties": { "x": { "type": "object" } } })]
        {
            assert!(Form::new("Go ahead?", &nothing, &[]).is_none(), "{nothing}");
        }
        let same = json!({ "properties": { "a": { "title": "Name" }, "b": { "title": "Name" } } });
        let form = Form::new("", &same, &[]).unwrap();
        assert_eq!(form.fields[1].question, "Name (b)", "two questions never share their words");
        assert_eq!(form.content("not json"), json!({}));
    }
}
