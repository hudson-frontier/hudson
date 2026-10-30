//! Atomic publication of trusted, validated configuration. A customer HTTP API
//! must construct this configuration through its own restricted capability types.
use crate::{
    configured::Configuration,
    definitions::digest,
    models::{now, Actor, VersionRef},
    storage::Store,
    Error, Result,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicationReceipt {
    pub agent_ref: VersionRef,
    pub digest: String,
    pub created_at: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PublishedConfiguration {
    pub actor_id: String,
    pub receipt: PublicationReceipt,
    pub frozen: serde_json::Value,
}
fn invalid(error: Box<dyn std::error::Error>) -> Error {
    Error::Invalid(error.to_string())
}

impl Store {
    /// Trusted host entrypoint. Validates without execution credentials or remote IO.
    /// No live definition changes survive a failed publication.
    pub fn publish_configuration(
        &self,
        actor: &Actor,
        configuration: &Configuration,
        request_key: &str,
    ) -> Result<PublicationReceipt> {
        if actor.id.trim().is_empty() || actor.workspace_id.trim().is_empty() {
            return Err(Error::Denied);
        }
        if request_key.is_empty() || request_key.len() > 256 {
            return Err(Error::Invalid("publication request key length".into()));
        }
        let frozen = configuration.freeze_publication()?;
        let expected = digest(&frozen)?;
        // Complete validation before taking the live transaction lock.
        let validated = configuration
            .clone()
            .build_admission_tree(Store::default(), actor)
            .map_err(invalid)?;
        let reference = validated.reference.clone();
        drop(validated);
        self.transact(|data| {
            let retry_key = (
                actor.workspace_id.clone(),
                actor.id.clone(),
                request_key.to_owned(),
            );
            if let Some(receipt) = data.publication_keys.get(&retry_key) {
                return if receipt.digest == expected {
                    Ok(receipt.clone())
                } else {
                    Err(Error::Conflict(
                        "publication key reused with changed content".into(),
                    ))
                };
            }
            let identity = (actor.workspace_id.clone(), reference.clone());
            if let Some(existing) = data.publications.get(&identity) {
                if existing.actor_id != actor.id || existing.receipt.digest != expected {
                    return Err(Error::Conflict("published revision is immutable".into()));
                }
                let receipt = existing.receipt.clone();
                data.publication_keys.insert(retry_key, receipt.clone());
                return Ok(receipt);
            }
            if data.runs.values().any(|run| {
                run.meta.workspace_id == actor.workspace_id && run.agent_ref == reference
            }) {
                return Err(Error::Conflict(
                    "existing runs require a new publication version".into(),
                ));
            }
            // Reuse the normal immutable-definition checks against a private snapshot
            // of current state. This preserves live counters and revocation policy.
            let staging = Store::staging(data.clone());
            let tree = configuration
                .clone()
                .build_admission_tree(staging.clone(), actor)
                .map_err(invalid)?;
            drop(tree);
            let staged = staging.read(|snapshot| Ok(snapshot.clone()))?;
            // Only definition/binding fields may be committed. Run state, effects,
            // credentials, memory contents and other receipts remain untouched.
            data.agents = staged.agents;
            data.tools = staged.tools;
            data.policies = staged.policies;
            data.model_budgets = staged.model_budgets;
            data.model_bindings = staged.model_bindings;
            data.memory_bindings = staged.memory_bindings;
            data.context_policies = staged.context_policies;
            data.coordination_policies = staged.coordination_policies;
            let receipt = PublicationReceipt {
                agent_ref: reference,
                digest: expected,
                created_at: now(),
            };
            data.publications.insert(
                identity,
                PublishedConfiguration {
                    actor_id: actor.id.clone(),
                    receipt: receipt.clone(),
                    frozen,
                },
            );
            data.publication_keys.insert(retry_key, receipt.clone());
            Ok(receipt)
        })
    }

    /// Find the publication pinned by a run's root, including delegated descendants.
    pub fn published_root(&self, actor: &Actor, id: uuid::Uuid) -> Result<VersionRef> {
        Ok(self.publication_root_run(actor, id)?.agent_ref)
    }

    fn publication_root_run(&self, actor: &Actor, id: uuid::Uuid) -> Result<crate::models::Run> {
        self.read(|data| {
            let mut current = id;
            let mut visited = std::collections::BTreeSet::new();
            loop {
                if !visited.insert(current) {
                    return Err(Error::Conflict("cyclic run ancestry".into()));
                }
                let run = data.run(actor, current)?;
                if let Some(operation) = run.parent_operation {
                    current = data
                        .operations
                        .get(&operation)
                        .ok_or(Error::NotFound)?
                        .run_id;
                } else {
                    data.publications
                        .get(&(actor.workspace_id.clone(), run.agent_ref.clone()))
                        .filter(|record| record.actor_id == actor.id)
                        .ok_or(Error::NotFound)?;
                    return Ok(run.clone());
                }
            }
        })
    }

    /// Reconstruct the root submission's budget as well as its immutable revision.
    pub fn published_run_configuration(
        &self,
        actor: &Actor,
        id: uuid::Uuid,
    ) -> Result<(uuid::Uuid, Configuration)> {
        let root = self.publication_root_run(actor, id)?;
        let config = self
            .published_configuration(actor, &root.agent_ref)?
            .for_submission(actor, root.request_key.as_deref().unwrap_or(""))?;
        Ok((root.meta.id, config))
    }

    /// Atomically admit a published run and its per-submission budget. No remote IO.
    pub fn submit_published(
        &self,
        actor: &Actor,
        reference: &VersionRef,
        input: serde_json::Value,
        request_key: &str,
        target: crate::scheduling::ScheduleTarget,
    ) -> Result<uuid::Uuid> {
        if request_key.is_empty() || request_key.len() > 256 {
            return Err(Error::Invalid(
                "published submissions require a request key of 1 to 256 bytes".into(),
            ));
        }
        let config = self
            .published_configuration(actor, reference)?
            .for_submission(actor, request_key)?;
        self.transact(|data| {
            let staging = Store::staging(data.clone());
            let tree = config
                .build_admission_tree(staging.clone(), actor)
                .map_err(invalid)?;
            let id = tree.runtime.submit_scheduled(
                actor,
                tree.reference.clone(),
                input,
                Some(request_key.to_owned()),
                tree.goal.clone(),
                Some(target),
            )?;
            *data = staging.read(|snapshot| Ok(snapshot.clone()))?;
            Ok(id)
        })
    }

    /// Restore a pinned publication without reading its original files or secrets.
    pub fn import_published_turn(
        &self,
        actor: &Actor,
        reference: &VersionRef,
        mut turn: crate::conversations::ImportedTurn,
        target: crate::scheduling::ScheduleTarget,
    ) -> Result<uuid::Uuid> {
        let config = self
            .published_configuration(actor, reference)?
            .for_submission(actor, &turn.request_key)?;
        self.transact(|data| {
            let staging = Store::staging(data.clone());
            let tree = config
                .build_admission_tree(staging.clone(), actor)
                .map_err(invalid)?;
            turn.goal = tree.goal.clone();
            let id = tree
                .runtime
                .import_turn(actor, tree.reference.clone(), turn, Some(target))?;
            *data = staging.read(|snapshot| Ok(snapshot.clone()))?;
            Ok(id)
        })
    }

    /// Restore a pinned publication without reading its original files or secrets.
    pub fn published_configuration(
        &self,
        actor: &Actor,
        reference: &VersionRef,
    ) -> Result<Configuration> {
        let record = self.read(|data| {
            data.publications
                .get(&(actor.workspace_id.clone(), reference.clone()))
                .filter(|record| record.actor_id == actor.id)
                .cloned()
                .ok_or(Error::NotFound)
        })?;
        if digest(&record.frozen)? != record.receipt.digest {
            return Err(Error::Conflict("publication integrity mismatch".into()));
        }
        Configuration::restore_publication(record.frozen).map_err(invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn actor() -> Actor {
        Actor {
            workspace_id: "company".into(),
            id: "owner".into(),
        }
    }
    fn source(name: &str) -> Value {
        json!({"name":name,"instructions":"help","provider":"ollama","model":"fixture"})
    }
    fn configuration(source: Value) -> Configuration {
        let path =
            std::env::temp_dir().join(format!("hudson-publication-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, serde_json::to_vec(&source).unwrap()).unwrap();
        let result = Configuration::load(&path);
        std::fs::remove_file(path).unwrap();
        result.unwrap()
    }
    fn snapshot(store: &Store) -> Value {
        store.read(|data| Ok(serde_json::to_value(data)?)).unwrap()
    }

    #[test]
    fn revisions_retry_restore_and_isolate_owners() {
        let store = Store::default();
        let owner = actor();
        let config = configuration(source("agent"));
        let receipt = store
            .publish_configuration(&owner, &config, "first")
            .unwrap();
        assert_eq!(
            receipt,
            store
                .publish_configuration(&owner, &config, "first")
                .unwrap()
        );
        assert_eq!(
            receipt,
            store
                .publish_configuration(&owner, &config, "second")
                .unwrap()
        );
        let restored = store
            .published_configuration(&owner, &receipt.agent_ref)
            .unwrap();
        assert_eq!(
            config.freeze_publication().unwrap(),
            restored.freeze_publication().unwrap()
        );
        restored.build_temporal_tree(store.clone(), &owner).unwrap();
        let before = snapshot(&store);
        let mut changed = source("agent");
        changed["instructions"] = json!("changed");
        let changed = configuration(changed);
        assert!(store
            .publish_configuration(&owner, &changed, "first")
            .is_err());
        assert!(store
            .publish_configuration(&owner, &changed, "third")
            .is_err());
        for other in [
            Actor {
                id: "other".into(),
                ..owner.clone()
            },
            Actor {
                workspace_id: "other".into(),
                ..owner.clone()
            },
        ] {
            assert!(matches!(
                store.published_configuration(&other, &receipt.agent_ref),
                Err(Error::NotFound)
            ));
        }
        assert_eq!(before, snapshot(&store));
    }

    #[test]
    fn nested_conflict_rolls_back_the_entire_publication() {
        let store = Store::default();
        let owner = actor();
        configuration(source("child"))
            .build_admission_tree(store.clone(), &owner)
            .unwrap();
        let before = snapshot(&store);
        let mut child = source("child");
        child["instructions"] = json!("conflicting child");
        let mut root = source("new-root");
        root["subagents"] = json!([child]);
        root["shared_model_budget"] = json!({"group":"new-budget","limit":10});
        assert!(store
            .publish_configuration(&owner, &configuration(root), "publish")
            .is_err());
        assert_eq!(before, snapshot(&store));
    }

    #[test]
    fn publication_preserves_live_state_and_rejects_legacy_run_adoption() {
        let store = Store::default();
        let owner = actor();
        let mut original = source("legacy");
        original["shared_model_budget"] = json!({"group":"shared","limit":10});
        let config = configuration(original);
        let tree = config
            .clone()
            .build_admission_tree(store.clone(), &owner)
            .unwrap();
        tree.runtime
            .submit(&owner, tree.reference.clone(), json!("existing task"), None)
            .unwrap();
        let issued = store
            .issue_api_token(owner.clone(), "existing token".into(), now() + 3600)
            .unwrap();
        store
            .transact(|data| {
                data.model_budgets
                    .get_mut(&(owner.workspace_id.clone(), "shared".into()))
                    .unwrap()
                    .admitted = 3;
                Ok(())
            })
            .unwrap();
        let before = snapshot(&store);
        assert!(matches!(
            store.publish_configuration(&owner, &config, "legacy"),
            Err(Error::Conflict(_))
        ));
        assert_eq!(before, snapshot(&store));
        let mut new = source("new-agent");
        new["shared_model_budget"] = json!({"group":"shared","limit":10});
        store
            .publish_configuration(&owner, &configuration(new), "new")
            .unwrap();
        let after = snapshot(&store);
        for field in [
            "runs",
            "operations",
            "events",
            "api_tokens",
            "model_budgets",
            "memories",
        ] {
            assert_eq!(before[field], after[field], "changed {field}");
        }
        assert_eq!(
            store.authenticate_api_token(issued.bearer()).unwrap(),
            owner
        );
    }

    #[test]
    fn loaded_skill_survives_removing_its_source() {
        let dir = std::env::temp_dir().join(format!("hudson-skill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("instructions.md"), "Use supporting evidence.").unwrap();
        std::fs::create_dir(dir.join("package")).unwrap();
        std::fs::write(dir.join("package/SKILL.md"), "---\nname: package\ndescription: Portable package\n---\nRead reference.md for details.").unwrap();
        std::fs::write(
            dir.join("package/reference.md"),
            "Keep this resource after publication.",
        )
        .unwrap();
        let mut source = source("portable");
        source["skill_packages"] = json!(["package"]);
        source["skill_files"] =
            json!([{"name":"analysis","description":"Analyze evidence","path":"instructions.md"}]);
        std::fs::write(dir.join("agent.json"), serde_json::to_vec(&source).unwrap()).unwrap();
        let config = Configuration::load(&dir.join("agent.json")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        let store = Store::default();
        let receipt = store
            .publish_configuration(&actor(), &config, "portable")
            .unwrap();
        let restored = store
            .published_configuration(&actor(), &receipt.agent_ref)
            .unwrap();
        assert_eq!(
            config.freeze_publication().unwrap(),
            restored.freeze_publication().unwrap()
        );
        restored.build_temporal_tree(store, &actor()).unwrap();
    }
}
