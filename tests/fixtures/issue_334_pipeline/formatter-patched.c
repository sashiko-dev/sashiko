/* Modified driver file reviewed by Sashiko. Media core is not in this diff. */

#define EINVAL 22
#define V4L2_SUBDEV_FL_STREAMS 1

struct v4l2_mbus_framefmt {
	unsigned int code;
};
struct v4l2_subdev_state;
struct v4l2_subdev;
struct v4l2_subdev_mbus_code_enum {
	unsigned int which;
	unsigned int pad;
	unsigned int stream;
	unsigned int code;
};
struct v4l2_subdev_format {
	unsigned int which;
	unsigned int pad;
	unsigned int stream;
	struct v4l2_mbus_framefmt format;
};
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

struct v4l2_mbus_framefmt *
v4l2_subdev_state_get_format(struct v4l2_subdev_state *state,
			     unsigned int pad, unsigned int stream);
int v4l2_subdev_call_enum_mbus_code(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code);
int v4l2_subdev_call_set_fmt(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_format *format);

static int formatter_subdev_enum_mbus_code(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code)
{
	struct v4l2_mbus_framefmt *fmt;

	fmt = v4l2_subdev_state_get_format(state, code->pad, code->stream);
	code->code = fmt->code;
	return 0;
}

static int formatter_subdev_set_fmt(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_format *format)
{
	struct v4l2_mbus_framefmt *fmt;

	fmt = v4l2_subdev_state_get_format(state, format->pad, format->stream);
	format->format = *fmt;
	return 0;
}

static struct v4l2_subdev_pad_ops formatter_pad_ops = {
	.enum_mbus_code = formatter_subdev_enum_mbus_code,
	.set_fmt = formatter_subdev_set_fmt,
};
static struct v4l2_subdev_ops formatter_ops = {
	.pad = &formatter_pad_ops,
};
static struct v4l2_subdev formatter_sd = {
	.flags = V4L2_SUBDEV_FL_STREAMS,
	.ops = &formatter_ops,
};

static int formatter_case_alpha(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code)
{
	struct v4l2_mbus_framefmt *fmt;

	fmt = v4l2_subdev_state_get_format(state, code->pad, code->stream);
	code->code = fmt->code;
	return 0;
}

static int invoke_case_alpha(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code)
{
	return formatter_case_alpha(sd, state, code);
}

static int formatter_case_beta(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code)
{
	struct v4l2_mbus_framefmt *fmt;

	fmt = v4l2_subdev_state_get_format(state, code->pad, code->stream);
	code->code = fmt->code;
	return 0;
}

static int invoke_case_beta_via_ops(
	struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code)
{
	static struct v4l2_subdev_pad_ops beta_pad_ops = {
		.enum_mbus_code = formatter_case_beta,
	};
	static struct v4l2_subdev_ops beta_ops = {
		.pad = &beta_pad_ops,
	};
	static struct v4l2_subdev beta_sd = {
		.flags = V4L2_SUBDEV_FL_STREAMS,
		.ops = &beta_ops,
	};

	return v4l2_subdev_call_enum_mbus_code(&beta_sd, state, code);
}

static int invoke_case_beta_direct(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code)
{
	return formatter_case_beta(sd, state, code);
}

static int formatter_case_gamma(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code)
{
	struct v4l2_mbus_framefmt *fmt;

	fmt = v4l2_subdev_state_get_format(state, code->pad + 1, code->stream);
	code->code = fmt->code;
	return 0;
}

static int invoke_case_gamma(
	struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code)
{
	static struct v4l2_subdev_pad_ops gamma_pad_ops = {
		.enum_mbus_code = formatter_case_gamma,
	};
	static struct v4l2_subdev_ops gamma_ops = {
		.pad = &gamma_pad_ops,
	};
	static struct v4l2_subdev gamma_sd = {
		.flags = V4L2_SUBDEV_FL_STREAMS,
		.ops = &gamma_ops,
	};

	return v4l2_subdev_call_enum_mbus_code(&gamma_sd, state, code);
}

int formatter_case_delta(
	struct v4l2_subdev *sd, struct v4l2_subdev_state *state,
	struct v4l2_subdev_mbus_code_enum *code)
{
	struct v4l2_mbus_framefmt *fmt;

	fmt = v4l2_subdev_state_get_format(state, code->pad, code->stream);
	code->code = fmt->code;
	return 0;
}

int formatter_exercise_paths(unsigned int scenario,
			     struct v4l2_subdev_state *state,
			     struct v4l2_subdev_mbus_code_enum *code)
{
	switch (scenario) {
	case 0:
		return invoke_case_alpha(&formatter_sd, state, code);
	case 1:
		return invoke_case_beta_direct(&formatter_sd, state, code);
	case 2:
		return invoke_case_beta_via_ops(state, code);
	case 3:
		return invoke_case_gamma(state, code);
	case 4:
		return formatter_case_delta(&formatter_sd, state, code);
	default:
		return -EINVAL;
	}
}
