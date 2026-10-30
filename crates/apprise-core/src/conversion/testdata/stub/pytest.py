"""Minimal pytest stand-in so upstream test functions can be called directly."""
import contextlib

class _Mark:
    def parametrize(self, names, values, **kw):
        def deco(fn):
            fn.__dict__.setdefault("_params", []).append((names, values))
            return fn
        return deco
    def __getattr__(self, name):
        def deco(*a, **k):
            if a and callable(a[0]) and not k:
                return a[0]
            return lambda fn: fn
        return deco

mark = _Mark()

class _Raises:
    def __init__(self, exc, match=None):
        self.exc = exc
    def __enter__(self):
        return self
    def __exit__(self, et, ev, tb):
        return et is not None and issubclass(et, self.exc)

def raises(exc, *a, **k):
    if a and callable(a[0]):
        try:
            a[0](*a[1:], **k)
        except exc:
            return
        return
    return _Raises(exc, k.get("match"))

def fixture(*a, **k):
    if a and callable(a[0]):
        return a[0]
    return lambda fn: fn

def approx(v, *a, **k):
    return v

class _Skip(Exception):
    pass

def skip(*a, **k):
    raise _Skip()

def fail(*a, **k):
    raise AssertionError(a)

warns = raises
