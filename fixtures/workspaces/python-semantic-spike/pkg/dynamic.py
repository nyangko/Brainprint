from .base import Base


def dispatch(target: Base, name: str):
    return getattr(target, name)()
