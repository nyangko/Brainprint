from .config import PROFILE_LOCALE


def build_profile(user_id: str, locale: str = PROFILE_LOCALE) -> dict[str, str]:
    return {"user_id": user_id, "locale": locale}
