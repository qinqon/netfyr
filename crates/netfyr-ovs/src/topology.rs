use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde_json::{Value as Json, json};

use crate::Failure;

/// A language-independent observation record, converted into State by the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub device_type: String,
    pub fields_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub device_name: Option<String>,
    pub field: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub devices: Vec<Device>,
    pub diagnostics: Vec<Diagnostic>,
}

struct Row {
    uuid: String,
    name: String,
    children: BTreeSet<String>,
    kind: String,
}

fn uuid(value: &Json) -> Result<String, Failure> {
    let pair = value
        .as_array()
        .filter(|pair| pair.len() == 2)
        .ok_or_else(|| Failure::undecodable("expected an OVSDB UUID atom"))?;
    let text = pair[1]
        .as_str()
        .filter(|_| pair[0] == "uuid")
        .ok_or_else(|| Failure::undecodable("expected an OVSDB UUID atom"))?;
    if text.len() != 36
        || !text.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(Failure::undecodable("invalid OVSDB UUID"));
    }
    Ok(text.to_ascii_lowercase())
}

fn uuid_set(value: &Json) -> Result<BTreeSet<String>, Failure> {
    if value.get(0) == Some(&json!("uuid")) {
        return Ok([uuid(value)?].into());
    }
    let pair = value
        .as_array()
        .filter(|pair| pair.len() == 2 && pair[0] == "set")
        .ok_or_else(|| Failure::undecodable("expected an OVSDB UUID set"))?;
    let values = pair[1]
        .as_array()
        .ok_or_else(|| Failure::undecodable("invalid OVSDB set"))?;
    let mut result = BTreeSet::new();
    for value in values {
        if !result.insert(uuid(value)?) {
            return Err(Failure::undecodable("duplicate UUID in OVSDB set"));
        }
    }
    Ok(result)
}

fn string(row: &Json, column: &str) -> Result<String, Failure> {
    row.get(column)
        .and_then(Json::as_str)
        .map(str::to_string)
        .ok_or_else(|| Failure::undecodable(&format!("missing or non-string {column} column")))
}

fn rows(
    result: &Json,
    children: Option<&str>,
    kind: Option<&str>,
    root: bool,
) -> Result<BTreeMap<String, Row>, Failure> {
    if result.get("error").is_some() {
        return Err(Failure::unsupported(
            "OVSDB select failed; required tables or columns may be unavailable",
        ));
    }
    let rows = result
        .get("rows")
        .and_then(Json::as_array)
        .ok_or_else(|| Failure::undecodable("OVSDB select result has no rows array"))?;
    let mut by_uuid = BTreeMap::new();
    let mut names = BTreeSet::new();
    for row in rows {
        let row = Row {
            uuid: uuid(&row["_uuid"])?,
            name: if root {
                "Open_vSwitch".to_string()
            } else {
                string(row, "name")?
            },
            children: children
                .map(|column| uuid_set(&row[column]))
                .transpose()?
                .unwrap_or_default(),
            kind: kind
                .map(|column| string(row, column))
                .transpose()?
                .unwrap_or_default(),
        };
        if row.name.is_empty() || !names.insert(row.name.clone()) || by_uuid.contains_key(&row.uuid)
        {
            return Err(Failure::undecodable(
                "empty or duplicate OVSDB row identity",
            ));
        }
        by_uuid.insert(row.uuid.clone(), row);
    }
    Ok(by_uuid)
}

fn child_names(row: &Row, children: &BTreeMap<String, Row>) -> Result<Vec<String>, Failure> {
    let mut names = row
        .children
        .iter()
        .map(|id| {
            children
                .get(id)
                .map(|child| child.name.clone())
                .ok_or_else(|| {
                    Failure::undecodable("OVSDB snapshot contains a dangling row reference")
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

fn parents<'a>(child: &str, rows: &'a BTreeMap<String, Row>) -> BTreeSet<&'a str> {
    rows.values()
        .filter(|row| row.children.contains(child))
        .map(|row| row.name.as_str())
        .collect()
}

fn parent_field(
    fields: &mut Json,
    field: &str,
    names: BTreeSet<&str>,
    row: &Row,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if names.len() == 1 {
        fields[field] = json!(names.first().unwrap());
    } else if names.len() > 1 {
        diagnostics.push(Diagnostic {
            device_name: Some(row.name.clone()),
            field: Some(field.to_string()),
            message: format!(
                "row {} has multiple {field} parents: {}",
                row.uuid,
                names.into_iter().collect::<Vec<_>>().join(", ")
            ),
        });
    }
}

fn device(row: &Row, device_type: &str, fields: Json) -> Device {
    Device {
        name: row.name.clone(),
        device_type: device_type.to_string(),
        fields_json: json!({device_type: fields}).to_string(),
    }
}

pub(crate) fn decode(results: &Json) -> Result<Snapshot, Failure> {
    let results = results
        .as_array()
        .filter(|results| results.len() == 4)
        .ok_or_else(|| Failure::undecodable("expected exactly four OVSDB select results"))?;
    let roots = rows(&results[0], Some("bridges"), None, true)?;
    let bridges = rows(&results[1], Some("ports"), Some("datapath_type"), false)?;
    let ports = rows(&results[2], Some("interfaces"), None, false)?;
    let interfaces = rows(&results[3], None, Some("type"), false)?;
    let mut snapshot = Snapshot::default();
    for row in roots.values() {
        snapshot.devices.push(device(
            row,
            "ovs_database",
            json!({
                "uuid": row.uuid, "bridges": child_names(row, &bridges)?
            }),
        ));
    }
    for row in bridges.values() {
        snapshot.devices.push(device(
            row,
            "ovs_bridge",
            json!({
                "uuid": row.uuid,
                "datapath_type": if row.kind.is_empty() { "system" } else { &row.kind },
                "ports": child_names(row, &ports)?
            }),
        ));
    }
    for row in ports.values() {
        let mut fields = json!({"uuid": row.uuid, "interfaces": child_names(row, &interfaces)?});
        parent_field(
            &mut fields,
            "bridge",
            parents(&row.uuid, &bridges),
            row,
            &mut snapshot.diagnostics,
        );
        snapshot.devices.push(device(row, "ovs_port", fields));
    }
    for row in interfaces.values() {
        let mut fields = json!({"uuid": row.uuid, "type": if row.kind.is_empty() { "system" } else { &row.kind }});
        parent_field(
            &mut fields,
            "port",
            parents(&row.uuid, &ports),
            row,
            &mut snapshot.diagnostics,
        );
        let bridge_names = ports
            .values()
            .filter(|port| port.children.contains(&row.uuid))
            .flat_map(|port| parents(&port.uuid, &bridges))
            .collect();
        parent_field(
            &mut fields,
            "bridge",
            bridge_names,
            row,
            &mut snapshot.diagnostics,
        );
        snapshot.devices.push(device(row, "ovs_interface", fields));
    }
    let rank = |kind: &str| match kind {
        "ovs_database" => 0,
        "ovs_bridge" => 1,
        "ovs_port" => 2,
        _ => 3,
    };
    snapshot
        .devices
        .sort_by(|a, b| (rank(&a.device_type), &a.name).cmp(&(rank(&b.device_type), &b.name)));
    snapshot.diagnostics.sort_by(|a, b| {
        (&a.device_name, &a.field, &a.message).cmp(&(&b.device_name, &b.field, &b.message))
    });
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Json {
        json!(["uuid", format!("00000000-0000-0000-0000-{n:012x}")])
    }

    fn fixture() -> Json {
        json!([
            {"rows": [{"_uuid": id(1), "bridges": id(2)}]},
            {"rows": [{"_uuid": id(2), "name":"br0", "ports":id(3), "datapath_type":""}]},
            {"rows": [{"_uuid": id(3), "name":"bond0", "interfaces":["set", [id(5), id(4)]]}]},
            {"rows": [
                {"_uuid": id(5), "name":"eth3", "type":""},
                {"_uuid": id(4), "name":"eth2", "type":"system"}
            ]}
        ])
    }

    #[test]
    fn topology_has_row_identities_normalized_types_and_sorted_relationships() {
        let snapshot = decode(&fixture()).unwrap();
        assert_eq!(snapshot.devices.len(), 5);
        let fields: Json = serde_json::from_str(&snapshot.devices[2].fields_json).unwrap();
        assert_eq!(fields["ovs_port"]["interfaces"], json!(["eth2", "eth3"]));
        assert_eq!(fields["ovs_port"]["bridge"], "br0");
        let fields: Json = serde_json::from_str(&snapshot.devices[3].fields_json).unwrap();
        assert_eq!(fields["ovs_interface"]["uuid"], id(4)[1]);
        assert_eq!(fields["ovs_interface"]["port"], "bond0");
        assert_eq!(fields["ovs_interface"]["bridge"], "br0");
        assert_eq!(fields["ovs_interface"]["type"], "system");
        let mut reordered = fixture();
        reordered[3]["rows"].as_array_mut().unwrap().reverse();
        reordered[2]["rows"][0]["interfaces"][1]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert_eq!(snapshot, decode(&reordered).unwrap());
    }

    #[test]
    fn ambiguous_parents_are_reported_without_picking_one() {
        let mut reply = fixture();
        reply[1]["rows"].as_array_mut().unwrap().push(json!({
            "_uuid": id(6), "name":"br1", "ports":id(3), "datapath_type":"netdev"
        }));
        let snapshot = decode(&reply).unwrap();
        assert_eq!(snapshot.diagnostics.len(), 3);
        for row in snapshot
            .devices
            .iter()
            .filter(|row| row.device_type == "ovs_port" || row.device_type == "ovs_interface")
        {
            let fields: Json = serde_json::from_str(&row.fields_json).unwrap();
            assert!(fields[&row.device_type].get("bridge").is_none());
        }
    }

    #[test]
    fn malformed_or_inconsistent_snapshots_fail_as_a_whole() {
        let mut reply = fixture();
        reply[2]["rows"][0]["interfaces"] = id(99);
        assert!(decode(&reply).is_err());
        let mut reply = fixture();
        reply[3]["rows"][0]["_uuid"] = json!(["uuid", "invalid"]);
        assert!(decode(&reply).is_err());
        let mut reply = fixture();
        reply[3]["rows"][0]["type"] = json!(null);
        assert!(decode(&reply).is_err());
        let mut reply = fixture();
        reply[3] = json!({"error":"syntax error"});
        assert_eq!(
            decode(&reply).unwrap_err().kind,
            crate::FailureKind::Unsupported
        );
    }
}
