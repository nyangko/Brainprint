import json

from .base import Base


class Impl(Base):
    def run(self, value: int) -> str:
        return json.dumps(value)


def call(x: Base):
    return x.run(1)
