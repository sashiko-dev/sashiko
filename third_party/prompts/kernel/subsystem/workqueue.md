- once queue_work() is called, flush_work() and other workqueue shutdown
methods prevent us from leaking the work struct on shutdown

