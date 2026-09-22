from .reexport import Exported


def register(handler):
    return handler


def wire():
    return register(Exported)
