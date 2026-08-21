//! Model-catalog flattening ⇐ pi `src/model-catalog.ts`.

use indexmap::IndexMap;

pub type ModelGroup<T = crate::types::Model> = IndexMap<String, T>;
pub type ModelGroups<T = crate::types::Model> = IndexMap<String, ModelGroup<T>>;
pub type ModelCatalog<T = crate::types::Model> = IndexMap<String, T>;

fn array_index(key: &str) -> Option<u32> {
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}

fn javascript_keys<T>(map: &IndexMap<String, T>) -> Vec<&String> {
    let mut indexes = map
        .keys()
        .filter_map(|key| array_index(key).map(|index| (index, key)))
        .collect::<Vec<_>>();
    indexes.sort_unstable_by_key(|(index, _)| *index);
    indexes
        .into_iter()
        .map(|(_, key)| key)
        .chain(map.keys().filter(|key| array_index(key).is_none()))
        .collect()
}

pub fn flatten_model_catalog<T: Clone>(
    _provider: &str,
    groups: &ModelGroups<T>,
) -> ModelCatalog<T> {
    let mut catalog = ModelCatalog::new();
    for group_name in javascript_keys(groups) {
        let group = &groups[group_name];
        for id in javascript_keys(group) {
            let model = &group[id];
            catalog.insert(id.clone(), model.clone());
        }
    }
    javascript_keys(&catalog)
        .into_iter()
        .map(|id| (id.clone(), catalog[id].clone()))
        .collect()
}

pub fn parse_embedded_model_catalog(provider: &str, json: &str) -> ModelCatalog {
    let groups: ModelGroups = serde_json::from_str(json)
        .unwrap_or_else(|error| panic!("invalid embedded model catalog for {provider}: {error}"));
    flatten_model_catalog(provider, &groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Model, ModelCost, ModelInput};

    fn model(id: &str, name: &str) -> Model {
        Model {
            id: id.to_owned(),
            name: name.to_owned(),
            api: "api".into(),
            provider: "provider".into(),
            base_url: "https://example.test".to_owned(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 1,
            max_tokens: 1,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    /// Pins pi `src/model-catalog.ts:27-33` `Object.assign` ordering.
    #[test]
    fn later_groups_overwrite_without_moving_existing_keys() {
        let groups = ModelGroups::from([
            (
                "first".to_owned(),
                ModelGroup::from([
                    ("a".to_owned(), model("a", "first-a")),
                    ("b".to_owned(), model("b", "b")),
                ]),
            ),
            (
                "second".to_owned(),
                ModelGroup::from([
                    ("a".to_owned(), model("a", "second-a")),
                    ("c".to_owned(), model("c", "c")),
                ]),
            ),
        ]);
        let catalog = flatten_model_catalog("ignored", &groups);
        assert_eq!(
            catalog.keys().map(String::as_str).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(catalog["a"].name, "second-a");
    }

    /// Pins pi `src/model-catalog.ts:26` JavaScript object-key enumeration.
    #[test]
    fn integer_like_object_keys_are_enumerated_numerically() {
        let groups = ModelGroups::from([
            (
                "10".to_owned(),
                ModelGroup::from([("20".to_owned(), model("20", "twenty"))]),
            ),
            (
                "2".to_owned(),
                ModelGroup::from([
                    ("10".to_owned(), model("10", "ten")),
                    ("1".to_owned(), model("1", "one")),
                    ("01".to_owned(), model("01", "leading-zero")),
                ]),
            ),
        ]);
        let catalog = flatten_model_catalog("ignored", &groups);
        assert_eq!(
            catalog.keys().map(String::as_str).collect::<Vec<_>>(),
            ["1", "10", "20", "01"]
        );
    }
}
