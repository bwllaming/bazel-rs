import sys
import unittest


class HelloTest(unittest.TestCase):
    def test_platform(self):
        self.assertIn(sys.platform, ("darwin", "linux"))


if __name__ == "__main__":
    unittest.main()
