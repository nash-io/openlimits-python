## python-openlimits

Starting point for Openlimits wrapper in Python using pyo3 for rust-python bindings. Currently supports only Nash.

### build and install the python package

Note that maturin requires that Rust/Cargo are installed on your system.

```bash
# clone this repo and cd in
git clone https://github.com/nash-io/openlimits-python.git
cd openlimits-python
# install the maturin build tool
pip install maturin
# this will build the python package 
maturin build --release

# you can then install similar to following:
pip install target/wheels/openlimits_python-0.1.0-cp38-cp38-macosx_10_7_x86_64.whl
```

### Example

See an example interaction [here](examples/openlimits-python-example.ipynb)
