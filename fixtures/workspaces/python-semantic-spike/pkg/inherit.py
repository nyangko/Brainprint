from typing import override

from . import base
from .base import Base


class Mixin:
    def run(self, value: int) -> str:
        return "mixin"

    def only_mixin(self) -> int:
        return 0


class Qualified(base.Base):
    def run(self, value: int) -> str:
        return "qualified"


class Multi(Base, Mixin):
    def run(self, value: int) -> str:
        return "multi"


class OnlyOne(Base, Mixin):
    def only_mixin(self) -> int:
        return 1


class Unrelated:
    def run(self, value: int) -> str:
        return "unrelated"


class Declared(Base):
    @override
    def run(self, value: int) -> str:
        return "declared"


class Orphan:
    @override
    def run(self, value: int) -> str:
        return "orphan"
