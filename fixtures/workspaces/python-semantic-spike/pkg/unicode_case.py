from .base import Base

변수 = "한글"


class 한글클래스(Base):
    def run(self, value: int) -> str:
        return 변수 + str(value)


def 호출(x: Base) -> str:
    return x.run(2)
