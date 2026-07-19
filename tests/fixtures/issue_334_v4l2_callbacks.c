struct format {
	unsigned int code;
};

struct state;
struct subdev;

struct request {
	unsigned int which;
	unsigned int pad;
	unsigned int stream;
	unsigned int code;
};

struct pad_ops {
	int (*enum_mbus_code)(struct subdev *sd, struct state *state,
			      struct request *request);
};

struct ops {
	struct pad_ops *pad;
};

struct subdev {
	unsigned int flags;
	struct ops *ops;
};

#define EINVAL 22
#define V4L2_SUBDEV_FL_STREAMS 1

struct format *v4l2_subdev_state_get_format(struct state *state,
					     unsigned int pad,
					     unsigned int stream);

static int check_state(struct subdev *sd, struct state *state,
		       unsigned int which, unsigned int pad,
		       unsigned int stream)
{
	if (sd->flags & V4L2_SUBDEV_FL_STREAMS) {
#if defined(CONFIG_VIDEO_V4L2_SUBDEV_API)
		if (!v4l2_subdev_state_get_format(state, pad, stream))
			return -EINVAL;
		return 0;
#else
		return -EINVAL;
#endif
	}

	return stream ? -EINVAL : 0;
}

/* Case A: the wrapper validates the same state/pad/stream before dispatch. */
static int guarded_safe_callback(struct subdev *sd, struct state *state,
				 struct request *request)
{
	struct format *fmt;

	fmt = v4l2_subdev_state_get_format(state, request->pad,
					   request->stream);
	request->code = fmt->code;
	return 0;
}

static struct pad_ops guarded_safe_pad_ops = {
	.enum_mbus_code = guarded_safe_callback,
};

static struct ops guarded_safe_ops = {
	.pad = &guarded_safe_pad_ops,
};

static struct subdev guarded_safe_sd = {
	.flags = V4L2_SUBDEV_FL_STREAMS,
	.ops = &guarded_safe_ops,
};

static int call_guarded_safe(struct state *state, struct request *request)
{
	struct subdev *sd = &guarded_safe_sd;

	return check_state(sd, state, request->which, request->pad,
			   request->stream) ?:
	       sd->ops->pad->enum_mbus_code(sd, state, request);
}

/* Case B: a direct call provides no caller-side non-NULL precondition. */
static int genuinely_unsafe_callback(struct subdev *sd, struct state *state,
				     struct request *request)
{
	struct format *fmt;

	fmt = v4l2_subdev_state_get_format(state, request->pad,
					   request->stream);
	request->code = fmt->code;
	return 0;
}

static int call_genuinely_unsafe(struct subdev *sd, struct state *state,
				struct request *request)
{
	return genuinely_unsafe_callback(sd, state, request);
}

/* Case C: one caller is guarded but another valid caller bypasses the guard. */
static int bypassable_callback(struct subdev *sd, struct state *state,
			       struct request *request)
{
	struct format *fmt;

	fmt = v4l2_subdev_state_get_format(state, request->pad,
					   request->stream);
	request->code = fmt->code;
	return 0;
}

static int call_bypassable_guarded(struct subdev *sd, struct state *state,
				   struct request *request)
{
	if (check_state(sd, state, request->which, request->pad,
			request->stream))
		return -EINVAL;

	return bypassable_callback(sd, state, request);
}

static int call_bypassable_direct(struct subdev *sd, struct state *state,
				  struct request *request)
{
	return bypassable_callback(sd, state, request);
}

/* Case D: the guard checks pad, but the callback dereferences pad + 1. */
static int mismatched_values_callback(struct subdev *sd, struct state *state,
				      struct request *request)
{
	struct format *fmt;

	fmt = v4l2_subdev_state_get_format(state, request->pad + 1,
					   request->stream);
	request->code = fmt->code;
	return 0;
}

static int call_mismatched_values(struct subdev *sd, struct state *state,
				  struct request *request)
{
	if (check_state(sd, state, request->which, request->pad,
			request->stream))
		return -EINVAL;

	return mismatched_values_callback(sd, state, request);
}

/* Case E: no caller context is supplied, so safety cannot be invented. */
static int missing_context_callback(struct subdev *sd, struct state *state,
				    struct request *request)
{
	struct format *fmt;

	fmt = v4l2_subdev_state_get_format(state, request->pad,
					   request->stream);
	request->code = fmt->code;
	return 0;
}
