import unittest

from greet import greet


class GreetTest(unittest.TestCase):
    def test_greet(self):
        self.assertEqual(greet("Ada"), "Hello, Ada")


if __name__ == "__main__":
    unittest.main()
