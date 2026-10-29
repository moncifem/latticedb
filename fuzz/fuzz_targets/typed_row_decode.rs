#![no_main]
use lattice_engines::typed::{self, ColType, Column, TableLayout};
use libfuzzer_sys::fuzz_target;

fn column(name: &str, ty: ColType, nullable: bool) -> Column {
    Column {
        name: name.into(),
        ty,
        nullable,
    }
}

fn layout(selector: u8) -> TableLayout {
    let columns = match selector % 21 {
        0 => Vec::new(), // Exercises tagged overflow values without schema columns.
        1 => vec![
            column("a_bool", ColType::Bool, true),
            column("b_int", ColType::Int, true),
            column("c_text", ColType::Text, true),
            column("d_bytes", ColType::Bytes, true),
            column("e_ref", ColType::Ref, true),
            column(
                "f_enum",
                ColType::Enum(vec!["todo".into(), "done".into()]),
                true,
            ),
            column("g_list", ColType::List(Box::new(ColType::Bool)), true),
        ],
        2 => vec![column("required", ColType::Bool, false)],
        depth => {
            let mut ty = ColType::Bool;
            // Bounded layouts include nesting immediately below and above the codec's limit.
            for _ in 0..depth - 2 {
                ty = ColType::List(Box::new(ty));
            }
            vec![column("nested", ty, false)]
        }
    };
    TableLayout {
        type_name: "FuzzRow".into(),
        columns,
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };
    let layout = layout(selector);
    if let Ok(value) = typed::decode_row(&layout, payload) {
        let encoded =
            typed::encode_row(&layout, &value).expect("a decoded typed row must be encodable");
        assert_eq!(
            encoded, payload,
            "successful decoding must preserve canonical bytes"
        );
        assert_eq!(typed::decode_row(&layout, &encoded), Ok(value));
    }
});
