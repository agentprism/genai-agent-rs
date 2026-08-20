//! Provider-scoped environment lookup ⇐ pi `src/utils/provider-env.ts`.

use crate::types::ProviderEnv;

pub fn get_provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    env.and_then(|values| values.get(name))
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::get_provider_env_value;
    use std::collections::BTreeMap;

    /// Derived from pi `src/utils/provider-env.ts:41-51`.
    #[test]
    fn scoped_nonempty_values_precede_process_values() {
        let env = BTreeMap::from([("PATH".to_owned(), "scoped".to_owned())]);
        assert_eq!(
            get_provider_env_value("PATH", Some(&env)).as_deref(),
            Some("scoped")
        );

        let empty = BTreeMap::from([("PATH".to_owned(), String::new())]);
        assert_eq!(
            get_provider_env_value("PATH", Some(&empty)),
            std::env::var("PATH").ok().filter(|value| !value.is_empty())
        );
        assert_eq!(
            get_provider_env_value("__AGENTPRISM_PI_MISSING_ENV__", None),
            None
        );
    }
}
