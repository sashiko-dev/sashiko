/* Unmodified media-core file present in the baseline commit. */

#define EINVAL 22
#define V4L2_SUBDEV_FL_STREAMS 1

struct v4l2_subdev_state;
struct v4l2_mbus_framefmt {
	unsigned int code;
};
struct v4l2_subdev_mbus_code_enum {
	unsigned int which;
	unsigned int pad;
	unsigned int stream;
};
struct v4l2_subdev_format {
	unsigned int which;
	unsigned int pad;
	unsigned int stream;
	struct v4l2_mbus_framefmt format;
};

struct v4l2_subdev;
struct v4l2_subdev_pad_ops {
	int (*enum_mbus_code)(struct v4l2_subdev *sd,
			      struct v4l2_subdev_state *state,
			      struct v4l2_subdev_mbus_code_enum *code);
	int (*set_fmt)(struct v4l2_subdev *sd,
		       struct v4l2_subdev_state *state,
		       struct v4l2_subdev_format *format);
};
struct v4l2_subdev_ops {
	struct v4l2_subdev_pad_ops *pad;
};
struct v4l2_subdev {
	unsigned int flags;
	struct v4l2_subdev_ops *ops;
};

void *v4l2_subdev_state_get_format(struct v4l2_subdev_state *state,
				   unsigned int pad,
				   unsigned int stream);

/* CORE_WRAPPER_PROOF_MARKER: this evidence must be retrieved through a tool. */
static int check_state(struct v4l2_subdev *sd,
		       struct v4l2_subdev_state *state,
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

static int call_enum_mbus_code(struct v4l2_subdev *sd,
			       struct v4l2_subdev_state *state,
			       struct v4l2_subdev_mbus_code_enum *code)
{
	if (!code)
		return -EINVAL;

	return check_state(sd, state, code->which, code->pad, code->stream) ?:
	       sd->ops->pad->enum_mbus_code(sd, state, code);
}

static int check_format(struct v4l2_subdev *sd,
			struct v4l2_subdev_state *state,
			struct v4l2_subdev_format *format)
{
	if (!format)
		return -EINVAL;

	return check_state(sd, state, format->which, format->pad,
			   format->stream);
}

static int call_set_fmt(struct v4l2_subdev *sd,
			struct v4l2_subdev_state *state,
			struct v4l2_subdev_format *format)
{
	return check_format(sd, state, format) ?:
	       sd->ops->pad->set_fmt(sd, state, format);
}

int v4l2_subdev_call_enum_mbus_code(struct v4l2_subdev *sd,
				    struct v4l2_subdev_state *state,
				    struct v4l2_subdev_mbus_code_enum *code)
{
	return call_enum_mbus_code(sd, state, code);
}

int v4l2_subdev_call_set_fmt(struct v4l2_subdev *sd,
			     struct v4l2_subdev_state *state,
			     struct v4l2_subdev_format *format)
{
	return call_set_fmt(sd, state, format);
}
