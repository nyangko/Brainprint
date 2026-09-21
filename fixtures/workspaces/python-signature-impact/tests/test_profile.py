from profile_app.profile import build_profile


def test_build_profile_uses_requested_locale() -> None:
    assert build_profile("u-1", locale="ko") == {"user_id": "u-1", "locale": "ko"}
