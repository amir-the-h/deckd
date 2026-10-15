//! Elgato encoder "feedback layouts": the touch-strip segment is described as a
//! list of positioned items (text, pixmap, bar, gbar) whose values plugins set
//! with `setFeedback`.

use std::path::Path;

use serde_json::{json, Map, Value};

#[derive(Debug, Clone)]
pub struct Item {
    pub key: String,
    pub kind: ItemKind,
    pub rect: [i32; 4],
    pub z: i64,
    pub props: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Text,
    Pixmap,
    Bar,
    GBar,
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub items: Vec<Item>,
}

fn builtin(id: &str) -> Option<Value> {
    let title = json!({ "key": "title", "type": "text", "rect": [16, 10, 136, 24], "font": { "size": 16, "weight": 600 }, "alignment": "left", "zOrder": 1 });
    Some(match id {
        "$X1" => json!([
            { "key": "title", "type": "text", "rect": [16, 10, 168, 24], "font": { "size": 16, "weight": 600 }, "alignment": "center", "zOrder": 1 },
            { "key": "icon", "type": "pixmap", "rect": [76, 40, 48, 48], "zOrder": 1 }
        ]),
        "$A0" => json!([
            title,
            { "key": "full-canvas", "type": "pixmap", "rect": [0, 0, 200, 100], "zOrder": 0 }
        ]),
        "$A1" => json!([
            title,
            { "key": "icon", "type": "pixmap", "rect": [16, 40, 48, 48], "zOrder": 1 },
            { "key": "value", "type": "text", "rect": [76, 40, 108, 32], "font": { "size": 24, "weight": 600 }, "alignment": "right", "zOrder": 1 }
        ]),
        "$B1" => json!([
            title,
            { "key": "icon", "type": "pixmap", "rect": [16, 40, 48, 48], "zOrder": 1 },
            { "key": "value", "type": "text", "rect": [76, 40, 108, 32], "font": { "size": 24, "weight": 600 }, "alignment": "right", "zOrder": 1 },
            { "key": "indicator", "type": "bar", "rect": [76, 74, 108, 20], "value": 0, "subtype": 4, "border_w": 0, "zOrder": 1 }
        ]),
        "$B2" => json!([
            title,
            { "key": "icon", "type": "pixmap", "rect": [16, 40, 48, 48], "zOrder": 1 },
            { "key": "value", "type": "text", "rect": [76, 40, 108, 32], "font": { "size": 24, "weight": 600 }, "alignment": "right", "zOrder": 1 },
            { "key": "indicator", "type": "gbar", "rect": [76, 74, 108, 20], "value": 0, "subtype": 4, "bar_h": 12, "zOrder": 1 }
        ]),
        "$C1" => json!([
            title,
            { "key": "icon1", "type": "pixmap", "rect": [16, 14, 24, 24], "zOrder": 1 },
            { "key": "indicator1", "type": "bar", "rect": [48, 20, 136, 12], "value": 0, "subtype": 4, "border_w": 0, "zOrder": 1 },
            { "key": "icon2", "type": "pixmap", "rect": [16, 58, 24, 24], "zOrder": 1 },
            { "key": "indicator2", "type": "bar", "rect": [48, 64, 136, 12], "value": 0, "subtype": 4, "border_w": 0, "zOrder": 1 }
        ]),
        _ => return None,
    })
}

impl Layout {
    /// `spec` is a built-in id (`$B1`), a path to a layout JSON relative to the
    /// plugin directory, or an inline layout object.
    pub fn resolve(spec: &Value, plugin_dir: Option<&Path>) -> Layout {
        let items = match spec {
            Value::String(id) if id.starts_with('$') => builtin(id),
            Value::String(path) => plugin_dir
                .and_then(|d| std::fs::read_to_string(d.join(path)).ok())
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.get("items").cloned()),
            Value::Object(o) => o.get("items").cloned(),
            _ => None,
        }
        .or_else(|| builtin("$X1"))
        .unwrap();
        let mut items: Vec<Item> = items.as_array().into_iter().flatten().filter_map(parse_item).collect();
        items.sort_by_key(|i| i.z);
        Layout { items }
    }

    /// Applies a `setFeedback` payload: `{key: value}` or `{key: {value, color, ...}}`.
    pub fn apply(&mut self, feedback: &Map<String, Value>) {
        for item in &mut self.items {
            match feedback.get(&item.key) {
                Some(Value::Object(o)) => {
                    for (k, v) in o {
                        item.props.insert(k.clone(), v.clone());
                    }
                }
                Some(v) => {
                    item.props.insert("value".into(), v.clone());
                }
                None => {}
            }
        }
    }

    pub fn set_prop(&mut self, key: &str, prop: &str, value: Value) {
        if let Some(item) = self.items.iter_mut().find(|i| i.key == key) {
            item.props.insert(prop.into(), value);
        }
    }

    pub fn set_default(&mut self, key: &str, value: Value) {
        if let Some(item) = self.items.iter_mut().find(|i| i.key == key) {
            item.props.entry("value").or_insert(value);
        }
    }
}

fn parse_item(v: &Value) -> Option<Item> {
    let o = v.as_object()?;
    let kind = match o.get("type")?.as_str()? {
        "text" => ItemKind::Text,
        "pixmap" => ItemKind::Pixmap,
        "bar" => ItemKind::Bar,
        "gbar" => ItemKind::GBar,
        _ => return None,
    };
    let r = o.get("rect")?.as_array()?;
    let n = |i: usize| r.get(i).and_then(Value::as_f64).unwrap_or(0.0) as i32;
    Some(Item {
        key: o.get("key").and_then(Value::as_str).unwrap_or_default().to_string(),
        kind,
        rect: [n(0), n(1), n(2), n(3)],
        z: o.get("zOrder").and_then(Value::as_i64).unwrap_or(0),
        props: o.clone(),
    })
}
