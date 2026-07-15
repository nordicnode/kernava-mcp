class Calculator:
    def compute(self, a, b):
        return a + b

    @staticmethod
    def create():
        return Calculator()

    @property
    def value(self):
        return 42
