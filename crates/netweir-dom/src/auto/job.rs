//! Job adverts: the schema.org JobPosting on the page.

use serde_json::{Map, Value, json};

use super::fields as f;
use super::schema_org::{Source, Thing, of_type};
use super::{Builder, Page};

pub(crate) const FIELDS: &[&str] = &[
    "url",
    "job_posting_id",
    "date_published",
    "date_published_raw",
    "date_modified",
    "date_modified_raw",
    "valid_through",
    "valid_through_raw",
    "job_title",
    "headline",
    "job_location",
    "description",
    "description_html",
    "employment_type",
    "base_salary",
    "requirements",
    "hiring_organization",
    "job_start_date",
    "job_start_date_raw",
    "remote_status",
];

pub(crate) fn read(page: &Page<'_, '_>, item: &mut Builder) {
    let jobs = of_type(&page.things, "JobPosting");
    let mut chosen: Vec<&Thing> = Vec::new();
    for source in [Source::JsonLd, Source::Microdata] {
        let here: Vec<&&Thing> = jobs.iter().filter(|t| t.source == source).collect();
        if let Some(first) = here.first() {
            if here.len() > 1 {
                item.note(
                    "item",
                    format!("the page has {} job postings; read the first", here.len()),
                );
            }
            chosen.push(first);
        }
    }
    for job in &chosen {
        from_thing(page, job, item);
    }
    let state = super::js_state::job(page, item);
    let layout = super::dom::job(page, item);
    item.probability = if chosen.iter().any(|j| j.props.contains_key("title")) {
        0.95
    } else if !chosen.is_empty() {
        0.8
    } else {
        state.or(layout).unwrap_or(0.05)
    };
}

fn from_thing(page: &Page<'_, '_>, job: &Thing, item: &mut Builder) {
    let p = &job.props;
    let get = |k: &str| p.get(k).unwrap_or(&Value::Null);
    let src = |k: &str| page.source(job, k);
    let id = match get("identifier") {
        Value::Object(o) => o.get("value").and_then(f::text),
        other => f::text(other),
    };
    item.offer("job_posting_id", id.map(Value::String), src("identifier"));
    for (field, prop) in [
        ("date_published", "datePosted"),
        ("date_modified", "dateModified"),
        ("valid_through", "validThrough"),
        ("job_start_date", "jobStartDate"),
    ] {
        let raw = get(prop);
        item.offer(
            field,
            f::iso_date(raw, page.now).map(Value::String),
            src(prop),
        );
        let raw_field = match field {
            "date_published" => "date_published_raw",
            "date_modified" => "date_modified_raw",
            "valid_through" => "valid_through_raw",
            _ => "job_start_date_raw",
        };
        item.offer(raw_field, f::text(raw).map(Value::String), src(prop));
    }
    item.offer(
        "job_title",
        f::text(get("title")).map(Value::String),
        src("title"),
    );
    item.offer(
        "job_location",
        location(get("jobLocation")),
        src("jobLocation"),
    );
    let description = get("description");
    item.offer(
        "description",
        f::text(description).map(Value::String),
        src("description"),
    );
    if let Some(html) = description.as_str().filter(|d| d.contains('<')) {
        let cleaned = crate::content::clean_html_fragment(html);
        item.offer(
            "description_html",
            (!cleaned.is_empty()).then_some(Value::String(cleaned)),
            src("description"),
        );
    }
    item.offer(
        "employment_type",
        f::employment_type(get("employmentType")).map(Value::String),
        src("employmentType"),
    );
    item.offer(
        "base_salary",
        f::salary(get("baseSalary")),
        src("baseSalary"),
    );
    let requirements: Vec<Value> = [
        "qualifications",
        "experienceRequirements",
        "educationRequirements",
        "skills",
    ]
    .iter()
    .flat_map(|k| each(get(k)))
    .filter_map(|r| match r {
        Value::Object(o) => o
            .get("description")
            .or_else(|| o.get("name"))
            .and_then(f::text),
        other => f::text(other),
    })
    .map(Value::String)
    .collect();
    if !requirements.is_empty() {
        item.offer(
            "requirements",
            Some(Value::Array(requirements)),
            src("qualifications"),
        );
    }
    item.offer(
        "hiring_organization",
        organization(get("hiringOrganization")),
        src("hiringOrganization"),
    );
    let remote = each(get("jobLocationType"))
        .filter_map(Value::as_str)
        .any(|t| t.trim().eq_ignore_ascii_case("TELECOMMUTE"));
    if remote {
        item.offer(
            "remote_status",
            Some(json!("Remote")),
            src("jobLocationType"),
        );
    }
}

/// `{raw}`: a Place's address written out, or the text given.
fn location(v: &Value) -> Option<Value> {
    let place = each(v).next()?;
    let raw = match place {
        Value::Object(o) => match o.get("address") {
            Some(Value::Object(a)) => {
                let parts: Vec<String> = [
                    "streetAddress",
                    "addressLocality",
                    "addressRegion",
                    "postalCode",
                    "addressCountry",
                ]
                .iter()
                .filter_map(|k| match a.get(*k) {
                    Some(Value::Object(c)) => c.get("name").and_then(f::text),
                    Some(other) => f::text(other),
                    None => None,
                })
                .collect();
                (!parts.is_empty()).then(|| parts.join(", "))
            }
            Some(other) => f::text(other),
            None => o.get("name").and_then(f::text),
        },
        other => f::text(other),
    }?;
    Some(json!({ "raw": raw }))
}

/// `{name, name_raw}` from an Organization or a name.
fn organization(v: &Value) -> Option<Value> {
    let name = match each(v).next()? {
        Value::Object(o) => o.get("name").and_then(f::text),
        other => f::text(other),
    }?;
    let mut out = Map::new();
    out.insert("name".into(), Value::String(name.clone()));
    out.insert("name_raw".into(), Value::String(name));
    Some(Value::Object(out))
}

fn each(v: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    match v {
        Value::Array(list) => Box::new(list.iter()),
        Value::Null => Box::new(std::iter::empty()),
        one => Box::new(std::iter::once(one)),
    }
}
