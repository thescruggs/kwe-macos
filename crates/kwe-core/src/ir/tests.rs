// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use serde_json::json;

fn parse(value: &Value) -> SceneIr {
    parse_scene_ir(&serde_json::to_vec(value).unwrap()).expect("valid scene.json")
}

fn object_by_name<'a>(scene: &'a SceneIr, name: &str) -> &'a ObjectIr {
    scene
        .objects
        .iter()
        .find(|object| object.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no object named {name}"))
}

// ---------------------------------------------------------------------------
// general
// ---------------------------------------------------------------------------

#[test]
fn general_block_reads_known_fields_with_the_renderers_defaults() {
    let scene = parse(&json!({"general": {}, "objects": []}));
    assert_eq!(scene.schema_version, SCHEMA_VERSION);
    assert_eq!(scene.general.clear_color, [0.0, 0.0, 0.0, 1.0]);
    assert_eq!(scene.general.resolution, None);
    assert_eq!(scene.general.fps, None);
    assert_eq!(scene.general.script, None);
    assert!(scene.general.unknown.is_empty());
    assert!(scene.objects.is_empty());
    assert!(scene.unknown.is_empty());
}

#[test]
fn general_block_reads_authored_values() {
    let scene = parse(&json!({
        "general": {
            "clearcolor": "0.5 0.25 0.75",
            "resolution": [1920, 1080],
            "fps": 30.0,
            "script": "wallpaper.js"
        },
        "objects": []
    }));
    assert_eq!(scene.general.clear_color, [0.5, 0.25, 0.75, 1.0]);
    assert_eq!(scene.general.resolution, Some((1920, 1080)));
    assert_eq!(scene.general.fps, Some(30.0));
    assert_eq!(scene.general.script.as_deref(), Some("wallpaper.js"));
}

#[test]
fn resolution_wins_over_orthogonalprojection_and_the_loser_is_preserved_in_unknown() {
    let scene = parse(&json!({
        "general": {
            "resolution": [640, 480],
            "orthogonalprojection": {"width": 1280, "height": 720}
        },
        "objects": []
    }));
    assert_eq!(scene.general.resolution, Some((640, 480)));
    assert_eq!(
        scene.general.unknown.get("orthogonalprojection"),
        Some(&json!({"width": 1280, "height": 720}))
    );
}

#[test]
fn orthogonalprojection_is_used_when_resolution_is_absent() {
    let scene = parse(&json!({
        "general": {"orthogonalprojection": {"width": 800, "height": 600}},
        "objects": []
    }));
    assert_eq!(scene.general.resolution, Some((800, 600)));
    assert!(scene.general.unknown.is_empty());
}

#[test]
fn unknown_general_and_root_keys_are_preserved() {
    let scene = parse(&json!({
        "general": {"clearcolor": [0.0, 0.0, 0.0, 1.0], "camera": "not a real key"},
        "objects": [],
        "version": 2
    }));
    assert_eq!(
        scene.general.unknown.get("camera"),
        Some(&json!("not a real key"))
    );
    assert_eq!(scene.unknown.get("version"), Some(&json!(2)));
}

// ---------------------------------------------------------------------------
// per-family objects
// ---------------------------------------------------------------------------

#[test]
fn image_layer_tint_wins_over_color_and_color_survives_in_unknown() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 1, "name": "img", "image": "textures/a.png",
             "tint": [0.1, 0.2, 0.3, 0.4], "color": [9, 9, 9]}
        ]
    }));
    let object = object_by_name(&scene, "img");
    let ObjectKindIr::Image(image) = &object.kind else {
        panic!("expected Image, got {:?}", object.kind);
    };
    assert_eq!(image.image, "textures/a.png");
    assert_eq!(image.tint, [0.1, 0.2, 0.3, 0.4]);
    assert_eq!(object.unknown.get("color"), Some(&json!([9, 9, 9])));
}

#[test]
fn blend_mode_alias_blendmode_wins_over_colorblendmode() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 1, "name": "a", "image": "a.png", "blendMode": 2, "colorBlendMode": 5}
        ]
    }));
    let object = object_by_name(&scene, "a");
    assert_eq!(object.common.blend_mode, 2);
    assert_eq!(object.unknown.get("colorBlendMode"), Some(&json!(5)));
}

#[test]
fn colorblendmode_is_used_when_blendmode_is_absent() {
    let scene = parse(&json!({
        "general": {},
        "objects": [{"id": 1, "name": "a", "image": "a.png", "colorBlendMode": 7}]
    }));
    let object = object_by_name(&scene, "a");
    assert_eq!(object.common.blend_mode, 7);
    assert!(object.unknown.is_empty());
}

#[test]
fn model_and_texv_and_textureless_image_classify_and_preserve_raw_image() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 1, "name": "model", "image": "models/deco.json"},
            {"id": 2, "name": "texv", "image": "textures/a.tex"},
            {"id": 3, "name": "textureless", "image": 42}
        ]
    }));
    let model = object_by_name(&scene, "model");
    assert!(matches!(&model.kind, ObjectKindIr::Model(m) if m.model_ref == "models/deco.json"));

    let texv = object_by_name(&scene, "texv");
    assert!(matches!(&texv.kind, ObjectKindIr::TexvImage(i) if i.image == "textures/a.tex"));

    let textureless = object_by_name(&scene, "textureless");
    assert!(matches!(
        &textureless.kind,
        ObjectKindIr::TexturelessImage(_)
    ));
    // The non-string `image` value is not silently dropped: it survives
    // in the unknown bag since no typed field could represent it.
    assert_eq!(textureless.unknown.get("image"), Some(&json!(42)));
}

#[test]
fn video_object_reads_source_loop_and_rate() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 1, "name": "v", "video": "clips/a.mp4", "loop": false, "rate": 2.0,
             "size": [100, 50]}
        ]
    }));
    let object = object_by_name(&scene, "v");
    let ObjectKindIr::Video(video) = &object.kind else {
        panic!("expected Video, got {:?}", object.kind);
    };
    assert_eq!(video.source.as_deref(), Some("clips/a.mp4"));
    assert!(!video.loop_playback);
    assert_eq!(video.rate, 2.0);
    assert_eq!(video.size, [100.0, 50.0]);
}

#[test]
fn video_object_loop_default_is_true_and_tolerant_of_string_forms() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 1, "name": "default", "video": "a.mp4"},
            {"id": 2, "name": "stringfalse", "video": "b.mp4", "loop": "false"},
            {"id": 3, "name": "stringother", "video": "c.mp4", "loop": "yes"}
        ]
    }));
    let default_loop = object_by_name(&scene, "default");
    assert!(matches!(&default_loop.kind, ObjectKindIr::Video(v) if v.loop_playback));
    let string_false = object_by_name(&scene, "stringfalse");
    assert!(matches!(&string_false.kind, ObjectKindIr::Video(v) if !v.loop_playback));
    let string_other = object_by_name(&scene, "stringother");
    assert!(matches!(&string_other.kind, ObjectKindIr::Video(v) if v.loop_playback));
}

#[test]
fn text_object_reads_alignment_fallback_and_color() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 1, "name": "exact", "text": "hi", "horizontalalign": "right",
             "verticalalign": "bottom", "color": [0.1, 0.2, 0.3]},
            {"id": 2, "name": "fallback", "text": "hi", "alignment": "top-left"},
            {"id": 3, "name": "defaulted", "text": "hi"}
        ]
    }));

    let exact = object_by_name(&scene, "exact");
    let ObjectKindIr::Text(text) = &exact.kind else {
        panic!("expected Text");
    };
    assert_eq!(text.horizontal_align, HorizontalAlignIr::Right);
    assert_eq!(text.vertical_align, VerticalAlignIr::Bottom);
    assert_eq!(text.color, [0.1, 0.2, 0.3, 1.0]);

    let fallback = object_by_name(&scene, "fallback");
    let ObjectKindIr::Text(text) = &fallback.kind else {
        panic!("expected Text");
    };
    assert_eq!(text.horizontal_align, HorizontalAlignIr::Left);
    assert_eq!(text.vertical_align, VerticalAlignIr::Top);

    let defaulted = object_by_name(&scene, "defaulted");
    let ObjectKindIr::Text(text) = &defaulted.kind else {
        panic!("expected Text");
    };
    assert_eq!(text.horizontal_align, HorizontalAlignIr::Center);
    assert_eq!(text.vertical_align, VerticalAlignIr::Center);
    assert_eq!(text.color, [1.0, 1.0, 1.0, 1.0]);
}

#[test]
fn text_object_has_size_records_presence_and_preserves_the_raw_value() {
    let scene = parse(&json!({
        "general": {},
        "objects": [{"id": 1, "name": "t", "text": "hi", "size": [12, 34]}]
    }));
    let object = object_by_name(&scene, "t");
    let ObjectKindIr::Text(text) = &object.kind else {
        panic!("expected Text");
    };
    assert!(text.has_size);
    // scene.rs only ever checks presence for text's `size`; the number is
    // never read into a typed field, so it survives in the unknown bag.
    assert_eq!(object.unknown.get("size"), Some(&json!([12, 34])));
}

#[test]
fn particle_object_reads_known_fields_and_leaves_speed_fields_in_unknown() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 1, "name": "p", "particle": {
                "spawnRate": 20.0, "life": 2.0, "direction": 90.0, "spread": 15.0,
                "sizeStart": 4.0, "sizeEnd": 12.0, "alphaStart": 0.8, "alphaEnd": 0.1,
                "gravity": [0.0, -9.8], "colorStart": [1, 0, 0], "colorEnd": [0, 0, 1],
                "maxCount": 500, "texture": "textures/spark.png", "material": "ignored.png",
                "speed": 5.0, "speedMin": 1.0, "speedMax": 9.0,
                "instanceoverride": {"count": "0.5", "colorn": "0.2 0.4 0.6", "color": [9, 9, 9]}
            }}
        ]
    }));
    let object = object_by_name(&scene, "p");
    let ObjectKindIr::Particle(particle) = &object.kind else {
        panic!("expected Particle, got {:?}", object.kind);
    };
    assert_eq!(particle.spawn_rate, 20.0);
    assert_eq!(particle.life, 2.0);
    assert_eq!(particle.direction, 90.0);
    assert_eq!(particle.spread, 15.0);
    assert_eq!(particle.size_start, 4.0);
    assert_eq!(particle.size_end, 12.0);
    assert_eq!(particle.alpha_start, 0.8);
    assert_eq!(particle.alpha_end, 0.1);
    assert_eq!(particle.gravity, [0.0, -9.8]);
    assert_eq!(particle.color_start, [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(particle.color_end, [0.0, 0.0, 1.0, 1.0]);
    assert_eq!(particle.max_count, 500);
    // texture wins over material.
    assert_eq!(particle.material.as_deref(), Some("textures/spark.png"));
    assert_eq!(particle.instance_count, 0.5);
    // colorn wins over color, reduced to the mean of its 3 components.
    assert!((particle.instance_colorn - 0.4).abs() < 1e-6);

    // speed/speedMin/speedMax are deliberately untyped (module doc
    // departure (2)); material's loser ("material") also survives.
    let residue = object
        .unknown
        .get("particle")
        .and_then(Value::as_object)
        .expect("particle residue");
    assert_eq!(residue.get("speed"), Some(&json!(5.0)));
    assert_eq!(residue.get("speedMin"), Some(&json!(1.0)));
    assert_eq!(residue.get("speedMax"), Some(&json!(9.0)));
    assert_eq!(residue.get("material"), Some(&json!("ignored.png")));
    assert_eq!(
        residue.get("instanceoverride"),
        None,
        "instanceoverride was read"
    );
}

#[test]
fn particle_defaults_are_exactly_the_renderers_defaults() {
    // classify_scene_object only reaches Particle (not ParticleFile) when
    // the object names a texture/material; an otherwise-empty definition
    // exercises every OTHER field's default.
    let scene = parse(&json!({
        "general": {},
        "objects": [{"id": 1, "name": "p", "particle": {"texture": "p.png"}}]
    }));
    let object = object_by_name(&scene, "p");
    let ObjectKindIr::Particle(particle) = &object.kind else {
        panic!("expected Particle");
    };
    assert_eq!(particle.spawn_rate, 10.0);
    assert_eq!(particle.life, 1.0);
    assert_eq!(particle.direction, 0.0);
    assert_eq!(particle.spread, 0.0);
    assert_eq!(particle.size_start, 8.0);
    assert_eq!(particle.size_end, 8.0);
    assert_eq!(particle.alpha_start, 1.0);
    assert_eq!(particle.alpha_end, 0.0);
    assert_eq!(particle.gravity, [0.0, 0.0]);
    assert_eq!(particle.color_start, [1.0, 1.0, 1.0, 1.0]);
    assert_eq!(particle.color_end, [1.0, 1.0, 1.0, 1.0]);
    assert_eq!(particle.max_count, 1000);
    assert_eq!(particle.material.as_deref(), Some("p.png"));
    assert_eq!(particle.instance_count, 1.0);
    assert_eq!(particle.instance_colorn, 1.0);
}

#[test]
fn particle_file_object_reads_the_file_reference() {
    let scene = parse(&json!({
        "general": {},
        "objects": [{"id": 1, "name": "pf", "particle": "particles/embers.json"}]
    }));
    let object = object_by_name(&scene, "pf");
    assert!(
        matches!(&object.kind, ObjectKindIr::ParticleFile(p) if p.file_ref.as_deref() == Some("particles/embers.json"))
    );
}

#[test]
fn an_unclassifiable_object_is_the_unknown_kind_but_its_common_props_still_parse() {
    let scene = parse(&json!({
        "general": {},
        "objects": [{"id": 1, "name": "s", "sound": "audio/ambient.mp3", "alpha": 0.5}]
    }));
    let object = object_by_name(&scene, "s");
    assert_eq!(object.kind, ObjectKindIr::Unknown);
    assert_eq!(object.common.alpha, 0.5);
    // "sound" has no typed representation anywhere -- it lands in unknown.
    assert_eq!(
        object.unknown.get("sound"),
        Some(&json!("audio/ambient.mp3"))
    );
}

#[test]
fn visible_is_a_tri_state_bool_propertybound_or_absent() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 1, "name": "explicit", "image": "a.png", "visible": false},
            {"id": 2, "name": "wrapped", "image": "b.png",
             "visible": {"user": "vis", "value": true}},
            {"id": 3, "name": "bound", "image": "c.png",
             "visible": {"user": "vis", "value": {"nested": "not a bool"}}},
            {"id": 4, "name": "absent", "image": "d.png"}
        ]
    }));
    assert_eq!(
        object_by_name(&scene, "explicit").common.visible,
        VisibleIr::Bool(false)
    );
    assert_eq!(
        object_by_name(&scene, "wrapped").common.visible,
        VisibleIr::Bool(true)
    );
    assert_eq!(
        object_by_name(&scene, "bound").common.visible,
        VisibleIr::PropertyBound(json!({"nested": "not a bool"}))
    );
    assert_eq!(
        object_by_name(&scene, "absent").common.visible,
        VisibleIr::Absent
    );
}

#[test]
fn defaults_for_a_minimal_object_match_the_renderer_exactly() {
    let scene = parse(&json!({"general": {}, "objects": [{"image": "a.png"}]}));
    let object = &scene.objects[0];
    assert_eq!(object.name, None);
    assert_eq!(object.authored_id, None);
    assert_eq!(object.common.origin, [0.0, 0.0]);
    assert_eq!(object.common.angles, [0.0, 0.0, 0.0]);
    assert_eq!(object.common.scale, [1.0, 1.0]);
    assert_eq!(object.common.alpha, 1.0);
    assert_eq!(object.common.visible, VisibleIr::Absent);
    assert_eq!(object.common.blend_mode, 0);
    assert_eq!(object.common.brightness, 1.0);
    assert!(object.effects.is_empty());
    let ObjectKindIr::Image(image) = &object.kind else {
        panic!("expected Image");
    };
    assert_eq!(image.size, [0.0, 0.0]);
    assert_eq!(image.tint, [1.0, 1.0, 1.0, 1.0]);
}

// ---------------------------------------------------------------------------
// effects[]
// ---------------------------------------------------------------------------

#[test]
fn effects_entries_read_known_fields_and_preserve_unknown() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 1, "name": "m", "image": "models/deco.json", "effects": [
                {"id": 7, "name": "godrays", "visible": false,
                 "file": "effects/godrays.json", "passes": [{"target": "a"}],
                 "folder": "vendor/godrays"},
                {"file": "effects/tint.json"},
                {"name": "no file, skipped"}
            ]}
        ]
    }));
    let object = object_by_name(&scene, "m");
    // Deliberate departure from `sceneeffect::resolve_object_effects`
    // (module doc departure (1) applied to effects[] entries too): the
    // RENDERER skips a fileless entry outright, because that decision
    // requires resolving the effect FILE (I/O this pure scene.json parse
    // does not do) — but the ENTRY was still authored, so the IR keeps it
    // with `file: None` rather than dropping it silently.
    assert_eq!(
        object.effects.len(),
        3,
        "every authored entry is kept, even a fileless one"
    );
    let first = &object.effects[0];
    assert_eq!(first.id, 7);
    assert_eq!(first.name, "godrays");
    assert!(!first.visible);
    assert_eq!(first.file.as_deref(), Some("effects/godrays.json"));
    assert_eq!(first.passes, vec![json!({"target": "a"})]);
    assert_eq!(first.unknown.get("folder"), Some(&json!("vendor/godrays")));

    let second = &object.effects[1];
    assert_eq!(second.id, 0);
    assert_eq!(second.name, "");
    assert!(second.visible);
    assert_eq!(second.file.as_deref(), Some("effects/tint.json"));
    assert!(second.passes.is_empty());

    let third = &object.effects[2];
    assert_eq!(third.name, "no file, skipped");
    assert_eq!(third.file, None);
}

// ---------------------------------------------------------------------------
// StableId
// ---------------------------------------------------------------------------

#[test]
fn stable_id_assignment_and_duplicate_recording() {
    let scene = parse(&json!({
        "general": {},
        "objects": [
            {"id": 5, "name": "first"},
            {"name": "no-id"},
            {"id": 5, "name": "duplicate"},
            {"id": 6, "name": "second"}
        ]
    }));
    assert_eq!(scene.objects[0].stable_id, StableId::Authored(5));
    assert_eq!(scene.objects[1].stable_id, StableId::Index(1));
    assert_eq!(scene.objects[2].stable_id, StableId::Index(2));
    assert_eq!(scene.objects[3].stable_id, StableId::Authored(6));
    assert_eq!(
        scene.duplicate_ids,
        vec!["id 5 reused at index 2".to_string()]
    );
    // authored_id still reflects what was literally written, even for the
    // demoted duplicate.
    assert_eq!(scene.objects[2].authored_id, Some(5));
}

// ---------------------------------------------------------------------------
// Bounds and errors
// ---------------------------------------------------------------------------

fn objects_array(count: usize) -> Value {
    let objects: Vec<Value> = (0..count).map(|i| json!({"id": i as i64})).collect();
    json!({"general": {}, "objects": objects})
}

#[test]
fn exactly_max_objects_is_ok() {
    let bytes = serde_json::to_vec(&objects_array(MAX_OBJECTS)).unwrap();
    let scene = parse_scene_ir(&bytes).expect("exactly the cap must be accepted");
    assert_eq!(scene.objects.len(), MAX_OBJECTS);
}

#[test]
fn one_over_max_objects_is_refused_not_truncated() {
    let bytes = serde_json::to_vec(&objects_array(MAX_OBJECTS + 1)).unwrap();
    let error = parse_scene_ir(&bytes).expect_err("over the cap must be refused");
    assert!(matches!(error, IrError::ObjectsCap));
}

#[test]
fn a_non_object_entry_is_a_typed_error_naming_its_index() {
    let bytes = serde_json::to_vec(&json!({
        "general": {},
        "objects": [{"id": 1}, "not an object"]
    }))
    .unwrap();
    let error = parse_scene_ir(&bytes).expect_err("a non-object entry must be refused");
    assert!(matches!(
        error,
        IrError::ObjectEntryNotAnObject { index: 1 }
    ));
}

#[test]
fn invalid_json_is_a_parse_error() {
    let error = parse_scene_ir(b"{not json").expect_err("garbage bytes must be refused");
    assert!(matches!(error, IrError::Parse(_)));
}

#[test]
fn a_non_object_root_is_refused() {
    let bytes = serde_json::to_vec(&json!([1, 2, 3])).unwrap();
    let error = parse_scene_ir(&bytes).expect_err("an array root must be refused");
    assert!(matches!(error, IrError::NotAnObject));
}

#[test]
fn objects_missing_or_non_array_is_treated_as_empty() {
    let scene = parse(&json!({"general": {}}));
    assert!(scene.objects.is_empty());
    let scene = parse(&json!({"general": {}, "objects": "not an array"}));
    assert!(scene.objects.is_empty());
}

// ---------------------------------------------------------------------------
// Round trip and determinism
// ---------------------------------------------------------------------------

#[test]
fn determinism_same_bytes_parsed_twice_are_equal() {
    let bytes = serde_json::to_vec(&json!({
        "general": {"clearcolor": [0.1, 0.2, 0.3, 1.0], "fps": 60.0},
        "objects": [
            {"id": 1, "name": "a", "image": "a.png", "tint": [1, 0, 0]},
            {"name": "b", "text": "hi"}
        ]
    }))
    .unwrap();
    let first = parse_scene_ir(&bytes).unwrap();
    let second = parse_scene_ir(&bytes).unwrap();
    assert_eq!(first, second);
}

#[test]
fn round_trip_through_to_raw_value_reproduces_an_equal_scene_ir() {
    let bytes = serde_json::to_vec(&json!({
        "general": {
            "clearcolor": "0.5 0.25 0.75",
            "resolution": [1920, 1080],
            "orthogonalprojection": {"width": 100, "height": 100},
            "fps": 30.0,
            "script": "wallpaper.js",
            "camera": "unknown general key"
        },
        "objects": [
            {"id": 1, "name": "img", "image": "a.png", "tint": [1, 0, 0, 1], "color": [9, 9, 9],
             "blendMode": 2, "colorBlendMode": 5, "visible": false,
             "effects": [{"id": 1, "name": "e", "file": "e.json", "extra": "kept"}]},
            {"id": 2, "name": "model", "image": "m.json"},
            {"id": 3, "name": "texv", "image": "t.tex"},
            {"id": 4, "name": "textureless", "image": 42},
            {"id": 5, "name": "video", "video": "v.mp4", "loop": false, "rate": 2.0},
            {"id": 6, "name": "text", "text": "hi", "horizontalalign": "right",
             "verticalalign": "bottom", "color": [1, 1, 0], "size": [5, 6]},
            {"id": 7, "name": "particle", "particle": {
                "spawnRate": 5.0, "texture": "p.png", "material": "loser.png",
                "speed": 3.0, "speedMin": 1.0, "speedMax": 9.0,
                "instanceoverride": {"colorn": "0.5 0.5 0.5", "color": [9, 9, 9]}
            }},
            {"id": 8, "name": "particlefile", "particle": "particles/p.json"},
            {"id": 8, "name": "duplicate-id"},
            {"name": "sound", "sound": "a.mp3"},
            {"name": "extra-key", "image": "x.png", "vendorField": {"nested": true}}
        ],
        "unknownRoot": "kept"
    }))
    .unwrap();

    let first = parse_scene_ir(&bytes).unwrap();
    let raw = first.to_raw_value();
    let second = parse_scene_ir(&serde_json::to_vec(&raw).unwrap()).unwrap();
    assert_eq!(first, second, "raw={raw:#?}");

    // Spot-check the unknown-preserving path survived the round trip too,
    // not just structural equality.
    assert_eq!(second.unknown.get("unknownRoot"), Some(&json!("kept")));
    let img = object_by_name(&second, "img");
    assert_eq!(img.unknown.get("color"), Some(&json!([9, 9, 9])));
    assert_eq!(img.unknown.get("colorBlendMode"), Some(&json!(5)));
    assert_eq!(img.effects[0].unknown.get("extra"), Some(&json!("kept")));
    let particle = object_by_name(&second, "particle");
    let residue = particle
        .unknown
        .get("particle")
        .and_then(Value::as_object)
        .unwrap();
    assert_eq!(residue.get("speedMin"), Some(&json!(1.0)));
    assert_eq!(residue.get("material"), Some(&json!("loser.png")));
    assert_eq!(second.duplicate_ids, first.duplicate_ids);
}
