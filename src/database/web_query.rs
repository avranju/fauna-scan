//! Shared, parameterized explorer predicates. SQL fragments only come from enums.
use super::web_models::*;
type Params = Vec<Box<dyn std::any::Any + Send + Sync>>;
pub(super) fn predicates(f: &WebImageFilter, c: &str, postgres: bool) -> (Vec<String>, Params) {
    let mut conditions = vec!["1=1".to_owned()];
    let mut params: Params = vec![];
    let time = match f.advanced.time_field.as_str() {
        "discovered" => "images.discovered_at".to_owned(),
        "downloaded" => "images.downloaded_at".to_owned(),
        "classified" => format!("{c}.request_completed_at"),
        _ => "images.capture_start_at".to_owned(),
    };
    for (value, op) in [(&f.scope.from, ">="), (&f.scope.to, "<")] {
        if let Some(value) = value {
            conditions.push(format!("{time} {op} ?"));
            params.push(Box::new(super::format_timestamp(value)));
        }
    }
    if !f.scope.camera_ids.is_empty() {
        conditions.push(format!(
            "images.camera_id IN ({})",
            vec!["?"; f.scope.camera_ids.len()].join(",")
        ));
        for id in &f.scope.camera_ids {
            params.push(Box::new(*id));
        }
    }
    let download = f
        .download_status
        .as_ref()
        .map(|WebDownloadStatusFilter::OneOf(s)| s);
    let processing = f
        .processing_status
        .as_ref()
        .map(|WebProcessingStatusFilter::OneOf(s)| s);
    for (column, values) in [
        ("download_status", download),
        ("processing_status", processing),
    ] {
        if let Some(values) = values {
            conditions.push(format!(
                "images.{column} IN ({})",
                vec!["?"; values.len()].join(",")
            ));
            for v in values {
                params.push(Box::new(v.clone()));
            }
        }
    }
    if let Some(v) = &f.classified {
        conditions.push(format!(
            "{c}.id IS {}NULL",
            if matches!(v, WebClassifiedFilter::Classified) {
                "NOT "
            } else {
                ""
            }
        ));
    }
    for (column, value) in [
        ("contains_wildlife", f.contains_wildlife),
        ("is_interesting", f.is_interesting),
    ] {
        if let Some(v) = value {
            conditions.push(format!("{c}.{column} = {}", i32::from(v)));
        }
    }
    if let Some(v) = f.confidence_min {
        conditions.push(format!("{c}.confidence >= ?"));
        params.push(Box::new(v));
    }
    for (column, value) in [
        ("model", &f.advanced.model),
        ("prompt_version", &f.advanced.prompt_version),
    ] {
        if let Some(v) = value {
            conditions.push(format!("{c}.{column} = ?"));
            params.push(Box::new(v.clone()));
        }
    }
    if !f.advanced.species.is_empty() {
        let names = if postgres {
            format!(
                "SELECT lower(trim(s->>'name')) FROM jsonb_array_elements(COALESCE({c}.species_json, '[]')::jsonb) s"
            )
        } else {
            format!(
                "SELECT lower(trim(json_extract(s.value, '$.name'))) FROM json_each(COALESCE({c}.species_json, '[]')) s"
            )
        };
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM ({names}) species(name) WHERE name IN ({}))",
            vec!["?"; f.advanced.species.len()].join(",")
        ));
        // SQLite does not support column aliases after a derived-table name.
        if !postgres {
            conditions.pop();
            conditions.push(format!("EXISTS (SELECT 1 FROM json_each(COALESCE({c}.species_json, '[]')) s WHERE lower(trim(json_extract(s.value, '$.name'))) IN ({}))",vec!["?";f.advanced.species.len()].join(",")));
        }
        for s in &f.advanced.species {
            params.push(Box::new(s.trim().to_lowercase()));
        }
    }
    if let Some(q) = &f.advanced.text {
        conditions.push(format!("(lower(COALESCE(cameras.name,'')) LIKE ? ESCAPE '!' OR lower(COALESCE({c}.summary,'')) LIKE ? ESCAPE '!')"));
        let q = format!(
            "%{}%",
            q.to_lowercase()
                .replace('!', "!!")
                .replace('%', "!%")
                .replace('_', "!_")
        );
        params.push(Box::new(q.clone()));
        params.push(Box::new(q));
    }
    if let Some(failure) = &f.advanced.failure {
        conditions.push(if failure=="retryable" {"(images.download_status = 'retry_wait' OR images.processing_status = 'retry_wait')"}else{"(images.download_status IN ('failed','unavailable') OR images.processing_status IN ('failed','missing'))"}.to_owned());
    }
    (conditions, params)
}
pub(super) fn ordering(order: &WebImageOrder) -> (&'static str, &'static str) {
    match order {
        WebImageOrder::CapturedAscending => ("images.capture_start_at", "ASC"),
        WebImageOrder::CapturedDescending => ("images.capture_start_at", "DESC"),
        WebImageOrder::ConfidenceDescending => {
            ("COALESCE(classifications.confidence, -1.0)", "DESC")
        }
        WebImageOrder::ClassifiedDescending => {
            ("COALESCE(classifications.request_completed_at, '')", "DESC")
        }
        WebImageOrder::CameraAscending => ("cameras.channel_number", "ASC"),
        WebImageOrder::CameraDescending => ("cameras.channel_number", "DESC"),
        WebImageOrder::ConfidenceAscending => ("COALESCE(classifications.confidence, -1.0)", "ASC"),
        WebImageOrder::ClassifiedAscending => {
            ("COALESCE(classifications.request_completed_at, '')", "ASC")
        }
    }
}
#[cfg(feature = "postgres")]
pub(super) fn numbered(sql: &str) -> String {
    let mut n = 0;
    sql.chars()
        .map(|c| {
            if c == '?' {
                n += 1;
                format!("${n}")
            } else {
                c.to_string()
            }
        })
        .collect()
}
