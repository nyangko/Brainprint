from .profile import build_profile


def render_user(user_id: str) -> str:
    profile = build_profile(user_id)
    return f"{profile['user_id']}:{profile['locale']}"
