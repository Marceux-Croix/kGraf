"""Stack, Queue and RingBuffer: stubs. Each class docstring is the specification."""


class Stack:
    """A last-in, first-out stack.

    - `push(item)` adds an item on top.
    - `pop()` removes and returns the top item. It raises `IndexError` when the stack is empty.
    - `peek()` returns the top item without removing it. It raises `IndexError` when empty.
    - `len(stack)` is the number of items.
    - `is_empty()` is True when there are no items.
    """

    def __init__(self):
        raise NotImplementedError("Stack")


class Queue:
    """A first-in, first-out queue.

    - `enqueue(item)` adds an item at the back.
    - `dequeue()` removes and returns the front item. It raises `IndexError` when empty.
    - `peek()` returns the front item without removing it. It raises `IndexError` when empty.
    - `len(queue)` is the number of items.
    """

    def __init__(self):
        raise NotImplementedError("Queue")


class RingBuffer:
    """A fixed-capacity buffer that overwrites its oldest item when full.

    - `RingBuffer(capacity)` raises `ValueError` when `capacity < 1`.
    - `append(item)` adds an item; when the buffer is full the oldest item is dropped first.
    - `to_list()` returns the items from oldest to newest.
    - `len(buffer)` is the number of items held, never more than the capacity.
    - `is_full()` is True when the number of items equals the capacity.
    """

    def __init__(self, capacity):
        raise NotImplementedError("RingBuffer")
