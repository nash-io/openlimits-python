## python-openlimits

Starting point for Openlimits wrapper in Python using pyo3 for rust-python bindings. WIP. Currently only works for
the Nash openlimits client using the `pyo3` branch on openlimits

### build and install the python package

```bash
# first install the maturin build tool
pip install maturin
# this will build the python package 
maturin build

# you can then install similar to following:
# pip install target/wheels/nash_python-0.1.0-cp38-cp38-macosx_10_7_x86_64.whl
```

### use within python

```python
import nash_python
secret = "YOUR SECRET"
session = "YOUR SESSION"

client = nash_python.NashClient(secret, session)
```
