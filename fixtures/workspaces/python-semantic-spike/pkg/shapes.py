import abc
from typing import Optional, Protocol

from .base import Base


class Model:
    name: str = ""


class Abstract(abc.ABC):
    @abc.abstractmethod
    def compute(self) -> int:
        ...

    @classmethod
    def build(cls) -> "Abstract":
        raise NotImplementedError

    @staticmethod
    def helper() -> int:
        return 0

    @property
    def label(self) -> str:
        return ""


class Concrete(Abstract):
    def compute(self) -> int:
        return 1

    @classmethod
    def build(cls) -> "Abstract":
        return cls()

    @staticmethod
    def helper() -> int:
        return 1

    @property
    def label(self) -> str:
        return "concrete"


class Runner(Protocol):
    def run(self, value: int) -> str:
        ...


class DuckTyped:
    def run(self, value: int) -> str:
        return "duck"


def annotated(x: list[Model], y: Optional[Model]) -> dict[str, Model]:
    return {}


def plain(model: Model, seed: Base) -> Model:
    return model
