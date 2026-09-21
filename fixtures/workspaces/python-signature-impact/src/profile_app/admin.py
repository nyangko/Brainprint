from .profile import build_profile


def admin_preview(user_id: str) -> dict[str, str]:
    return build_profile(user_id, locale="en")
