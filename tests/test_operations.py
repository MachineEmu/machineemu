from machineemu.runtime.operations import OperationJournal


def test_operation_journal_survives_api_process_recreation(tmp_path):
    path = tmp_path / "runtime" / "operations.json"
    first = OperationJournal(path=path)
    operation = first.create("restart", "session-1")
    first.update(operation, "succeeded", result={"state": "running"})

    second = OperationJournal(path=path)
    restored = second.get(operation.operation_id)
    assert restored is not None
    assert restored.public()["state"] == "succeeded"
    assert restored.public()["result"] == {"state": "running"}
