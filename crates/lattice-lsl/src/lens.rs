//! Migration lenses: reversible data transforms that go with a schema change.
//!
//! A lens is a list of steps. [`Lens::up`] rewrites data written under the old schema so it is valid
//! under the new one, and [`Lens::down`] undoes it. [`Lens::check`] proves that the steps describe
//! exactly the difference between two checked schemas, so a lens cannot drift from the schema it
//! claims to migrate.

use crate::check::{Compiled, Field};
use crate::validate::Value;

/// One step of a migration, scoped to a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Renames a field. Lossless in both directions.
    Rename {
        /// Type the step applies to.
        ty: String,
        /// Old field name.
        from: String,
        /// New field name.
        to: String,
    },
    /// Adds a field with a default for existing data. `down` drops it again.
    AddField {
        /// Type the step applies to.
        ty: String,
        /// New field name.
        field: String,
        /// Value given to data that does not have the field.
        default: Value,
    },
    /// Drops a field. `down` can only restore it if a value to restore is declared, and that is lossy:
    /// the dropped data is gone.
    DropField {
        /// Type the step applies to.
        ty: String,
        /// Field to drop.
        field: String,
        /// Value `down` puts back, if the migration is meant to be reversible.
        restore: Option<Value>,
    },
}

/// Why a lens could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LensError {
    /// The value is not an object.
    #[error("a value of an entity type must be an object")]
    NotAnObject,
    /// A renamed or dropped field is not there.
    #[error("field `{0}` is missing")]
    MissingField(String),
    /// The target name of a rename or add is already taken.
    #[error("field `{0}` already exists")]
    FieldExists(String),
    /// `down` met a dropped field with nothing to restore.
    #[error("dropped field `{0}` cannot be restored: the migration declares no value")]
    NotReversible(String),
}

/// A migration for one schema change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lens {
    /// Steps in the order they apply going up.
    pub steps: Vec<Step>,
}

fn obj(v: Value) -> Result<std::collections::BTreeMap<String, Value>, LensError> {
    match v {
        Value::Object(o) => Ok(o),
        _ => Err(LensError::NotAnObject),
    }
}

impl Lens {
    /// Migrates data of type `ty` from the old schema to the new one.
    pub fn up(&self, ty: &str, value: Value) -> Result<Value, LensError> {
        let mut o = obj(value)?;
        for step in &self.steps {
            match step {
                Step::Rename { ty: t, from, to } if t == ty => {
                    if o.contains_key(to) {
                        return Err(LensError::FieldExists(to.clone()));
                    }
                    // An optional field may legitimately be absent, so only move what is there.
                    if let Some(v) = o.remove(from) {
                        o.insert(to.clone(), v);
                    }
                }
                Step::AddField {
                    ty: t,
                    field,
                    default,
                } if t == ty => {
                    o.entry(field.clone()).or_insert_with(|| default.clone());
                }
                Step::DropField { ty: t, field, .. } if t == ty => {
                    o.remove(field);
                }
                _ => {}
            }
        }
        Ok(Value::Object(o))
    }

    /// Migrates data of type `ty` from the new schema back to the old one.
    pub fn down(&self, ty: &str, value: Value) -> Result<Value, LensError> {
        let mut o = obj(value)?;
        for step in self.steps.iter().rev() {
            match step {
                Step::Rename { ty: t, from, to } if t == ty => {
                    if o.contains_key(from) {
                        return Err(LensError::FieldExists(from.clone()));
                    }
                    if let Some(v) = o.remove(to) {
                        o.insert(from.clone(), v);
                    }
                }
                Step::AddField { ty: t, field, .. } if t == ty => {
                    o.remove(field);
                }
                Step::DropField {
                    ty: t,
                    field,
                    restore,
                } if t == ty => match restore {
                    Some(v) => {
                        o.entry(field.clone()).or_insert_with(|| v.clone());
                    }
                    None => return Err(LensError::NotReversible(field.clone())),
                },
                _ => {}
            }
        }
        Ok(Value::Object(o))
    }

    /// Applies the steps to the **schema** and returns the problems that stop the result from being
    /// exactly `new`. An empty list means the lens migrates `old` to `new`, no more and no less.
    pub fn check(&self, old: &Compiled, new: &Compiled) -> Vec<String> {
        let mut types = old.types.clone();
        let mut bad = Vec::new();
        for step in &self.steps {
            match step {
                Step::Rename { ty, from, to } => match types.get_mut(ty) {
                    Some(fields) => match fields.remove(from) {
                        Some(f) => {
                            fields.insert(to.clone(), f);
                        }
                        None => bad.push(format!("rename: `{ty}.{from}` does not exist")),
                    },
                    None => bad.push(format!("rename: type `{ty}` does not exist")),
                },
                Step::AddField { ty, field, .. } => match types.get_mut(ty) {
                    Some(fields) => {
                        // The new field's type and optionality come from `new`; the lens only
                        // promises to make old data carry it.
                        if let Some(f) = new.types.get(ty).and_then(|t| t.get(field)) {
                            fields.insert(field.clone(), f.clone());
                        } else {
                            bad.push(format!("add: `{ty}.{field}` is not in the new schema"));
                        }
                    }
                    None => bad.push(format!("add: type `{ty}` does not exist")),
                },
                Step::DropField { ty, field, .. } => match types.get_mut(ty) {
                    Some(fields) => {
                        if fields.remove(field).is_none() {
                            bad.push(format!("drop: `{ty}.{field}` does not exist"));
                        }
                    }
                    None => bad.push(format!("drop: type `{ty}` does not exist")),
                },
            }
        }
        // Types that exist only in the new schema need no data migration.
        let migrated: std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, Field>,
        > = types.into_iter().collect();
        for (name, fields) in &new.types {
            if let Some(m) = migrated.get(name)
                && m != fields
            {
                bad.push(format!(
                    "type `{name}`: the steps do not produce the new field set"
                ));
            }
        }
        for name in migrated.keys() {
            if !new.types.contains_key(name) {
                bad.push(format!(
                    "type `{name}` is removed by the schema but not by the lens"
                ));
            }
        }
        bad
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::check;
    use crate::parser::parse;
    use crate::validate::validate;
    use proptest::prelude::*;

    fn c(src: &str) -> Compiled {
        check(&parse(src).unwrap()).unwrap()
    }

    const OLD: &str = "type Story { title: text max 50  points: int in 0..=100 optional  acceptance_criteria: text }";
    const NEW: &str = "type Story { title: text max 50  points: int in 0..=100 optional  acceptance: text  priority: int in 0..=3 }";

    fn lens() -> Lens {
        Lens {
            steps: vec![
                Step::Rename {
                    ty: "Story".into(),
                    from: "acceptance_criteria".into(),
                    to: "acceptance".into(),
                },
                Step::AddField {
                    ty: "Story".into(),
                    field: "priority".into(),
                    default: Value::Int(1),
                },
            ],
        }
    }

    fn story(title: &str, points: Option<i64>, ac: &str) -> Value {
        let mut o = std::collections::BTreeMap::new();
        o.insert("title".to_string(), Value::Text(title.into()));
        o.insert("acceptance_criteria".to_string(), Value::Text(ac.into()));
        if let Some(p) = points {
            o.insert("points".to_string(), Value::Int(p));
        }
        Value::Object(o)
    }

    #[test]
    fn the_lens_matches_the_schema_change_and_catches_a_wrong_one() {
        let (old, new) = (c(OLD), c(NEW));
        assert!(
            lens().check(&old, &new).is_empty(),
            "{:?}",
            lens().check(&old, &new)
        );
        let mut wrong = lens();
        wrong.steps.pop();
        assert!(
            !wrong.check(&old, &new).is_empty(),
            "a lens that forgets the added field must be rejected"
        );
        let mut bogus = lens();
        bogus.steps.push(Step::DropField {
            ty: "Story".into(),
            field: "nope".into(),
            restore: None,
        });
        assert!(!bogus.check(&old, &new).is_empty());
    }

    proptest! {
        #[test]
        fn up_output_is_valid_under_the_new_schema_and_down_restores_the_input(
            title in "[a-z]{0,50}", points in proptest::option::of(0i64..=100), ac in "[a-z ]{0,30}"
        ) {
            let (old, new) = (c(OLD), c(NEW));
            let before = story(&title, points, &ac);
            prop_assert!(validate(&old, "Story", &before).is_ok());
            let migrated = lens().up("Story", before.clone()).unwrap();
            prop_assert!(validate(&new, "Story", &migrated).is_ok(), "{:?}", validate(&new, "Story", &migrated));
            prop_assert_eq!(lens().down("Story", migrated).unwrap(), before, "lossless round trip");
        }
    }

    #[test]
    fn dropping_is_only_reversible_when_a_restore_value_is_declared() {
        let drop = |restore| Lens {
            steps: vec![Step::DropField {
                ty: "T".into(),
                field: "x".into(),
                restore,
            }],
        };
        let mut o = std::collections::BTreeMap::new();
        o.insert("x".to_string(), Value::Int(7));
        let v = Value::Object(o);
        let up = drop(Some(Value::Int(0))).up("T", v.clone()).unwrap();
        assert_eq!(
            drop(None).down("T", up.clone()),
            Err(LensError::NotReversible("x".into()))
        );
        // The restore value is a stand-in, not the original: dropping lost the 7.
        let back = drop(Some(Value::Int(0))).down("T", up).unwrap();
        assert_ne!(back, v);
    }

    #[test]
    fn other_types_and_bad_input_are_handled() {
        assert_eq!(
            lens().up("Story", Value::Int(1)),
            Err(LensError::NotAnObject)
        );
        let other = Value::Object(Default::default());
        assert_eq!(
            lens().up("Epic", other.clone()).unwrap(),
            other,
            "steps for another type do not touch it"
        );
    }
}
