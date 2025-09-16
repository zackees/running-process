"""
Setup file.
"""

from pathlib import Path

from setuptools import setup

URL = "https://github.com/zackees/running-process"
KEYWORDS = "suprocess running process"
HERE = Path(__file__).parent



if __name__ == "__main__":
    setup(
        maintainer="Zachary Vorhies",
        keywords=KEYWORDS,
        url=URL,
        package_data={"": ["assets/example.txt"]},
        include_package_data=True)

