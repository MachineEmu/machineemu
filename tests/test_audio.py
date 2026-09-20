from machineemu.runtime.audio import AudioClientRegistry


def test_audio_client_registry_binds_capture_to_the_same_principal():
    now = [100.0]
    registry = AudioClientRegistry(clock=lambda: now[0])
    first = registry.attach("instance", "session", "alice")
    second = registry.attach("instance", "session", "bob")
    assert registry.lookup("instance", "session", first.token, "mallory") is None
    assert registry.claim(first)
    assert not registry.claim(second)
    assert registry.claim(second, takeover=True)
    assert registry.capture_owner("instance", "session") == second.token
    assert not registry.release(first)
    assert registry.release(second)
    now[0] += 61
    assert registry.lookup("instance", "session", first.token, "alice") is None
