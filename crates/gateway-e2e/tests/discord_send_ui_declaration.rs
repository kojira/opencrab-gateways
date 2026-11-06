//! D-1110: core accepts the Discord gateway's operations with `send_ui` (A2UI restored at the
//! gateway layer). Core's own hello validation is run on the real declarations; nothing in core
//! reserves the name or rejects the schema.

#[test]
fn d1110_core_accepts_the_discord_operations_with_send_ui() {
    for reply_files in [false, true] {
        let declarations = opencrab_discord_gateway::ops::operation_declarations(reply_files);
        let decls = opencrab_extgate::operations::validate_operations(&declarations, &|_| false)
            .expect("core validates the declarations");
        let send_ui = decls
            .iter()
            .find(|d| d.name == "send_ui")
            .expect("send_ui is declared");
        assert_eq!(send_ui.policy.dispatch.as_str(), "inline");
        // D-1121: say is a valid utterance operation for core.
        let say = decls
            .iter()
            .find(|d| d.name == "say")
            .expect("say is declared");
        assert_eq!(say.policy.dispatch.as_str(), "utterance");
        opencrab_extgate::operations::validate_runtime_compatibility(
            &decls,
            opencrab_extgate::operations::FinalDelivery::OperationDriven,
        )
        .expect("still an operation-driven runtime");
    }
}
