use super::*;

#[test]
fn configuration_is_strict_and_units_cannot_be_ambiguous() {
    let valid = json!({"unit":{"kind":"money","currency":"CNY"},"auth":{"kind":"none"},"query_url":"https://example.invalid/balance","script":"return {value: '12.34'};"});
    let config: Config = serde_json::from_value(valid.clone()).unwrap();
    assert!(validate(&config).is_ok());
    for unit in [
        json!({"kind":"money","currency":"CN"}),
        json!({"kind":"money","currency":"usd"}),
    ] {
        let mut v = valid.clone();
        v["unit"] = unit;
        assert!(validate(&serde_json::from_value(v).unwrap()).is_err());
    }
    let mut v = valid;
    v["unit"] = json!({"kind":"percentage","currency":"USD"});
    assert!(serde_json::from_value::<Config>(v).is_err());
}
#[test]
fn the_key_goes_where_the_configuration_says_and_nowhere_unsafe() {
    let with = |auth: Value| {
        let config = json!({"unit":{"kind":"percentage"},"auth":auth,"query_url":"https://example.invalid/balance","script":"return {value: 1};"});
        serde_json::from_value::<Config>(config)
            .map_err(|e| e.to_string())
            .and_then(|config| validate(&config).map_err(|e| e.message))
    };
    for good in [
        json!({"kind":"none"}),
        json!({"kind":"header","name":"Authorization","value":"Bearer {key}"}),
        json!({"kind":"header","name":"X-Token","value":"{key}"}),
        json!({"kind":"query","name":"api_key"}),
    ] {
        assert!(with(good.clone()).is_ok(), "{good}");
    }
    for bad in [
        json!("bearer"),
        json!({"kind":"header","name":"Bad Header","value":"{key}"}),
        json!({"kind":"header","name":"Cookie","value":"{key}"}),
        json!({"kind":"header","name":"host","value":"{key}"}),
        json!({"kind":"header","name":"X-Token","value":"Bearer"}),
        json!({"kind":"header","name":"X-Token","value":"{key}{key}"}),
        json!({"kind":"header","name":"X-Token","value":"{key}\r\nX-Other: 1"}),
        json!({"kind":"query","name":""}),
        json!({"kind":"query","name":"api key"}),
        json!({"kind":"query","name":"k","value":"{key}"}),
    ] {
        assert!(with(bad.clone()).is_err(), "{bad}");
    }
}
#[test]
fn errors_preserve_reading_and_age_marks_it_stale() {
    let reading = Reading {
        value: "12.34".into(),
        unit: Unit::Money {
            currency: "USD".into(),
        },
        checked_at: Utc::now(),
    };
    let view: View = Document {
        reading: Some(reading.clone()),
        error: Some("failed".into()),
        ..Default::default()
    }
    .into();
    assert!(view.stale);
    assert_eq!(view.document.reading.unwrap().value, "12.34");
    let view: View = Document {
        reading: Some(Reading {
            checked_at: Utc::now() - chrono::Duration::minutes(6),
            ..reading
        }),
        ..Default::default()
    }
    .into();
    assert!(view.stale);
}
