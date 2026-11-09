#include <metal_stdlib>
using namespace metal;

// One textured quad. `rect` is the picture's corners in clip space
// (left, top, right, bottom).
struct PictureUniforms {
    float4 rect;
    float2 textureSize;   // luma texels
    float fullRange;      // 1: full-range NV12, 0: video range
    float exact;          // 1: integer device pixels per picture pixel
};

struct QuadOut {
    float4 position [[position]];
    float2 uv;
};

vertex QuadOut picture_vertex(uint vid [[vertex_id]], constant PictureUniforms &u [[buffer(0)]]) {
    const float2 corners[4] = { float2(0, 0), float2(1, 0), float2(0, 1), float2(1, 1) };
    float2 c = corners[vid];
    QuadOut out;
    out.position = float4(mix(u.rect.x, u.rect.z, c.x), mix(u.rect.y, u.rect.w, c.y), 0, 1);
    out.uv = c;
    return out;
}

// BT.709 YCbCr to R'G'B' (the host encodes BT.709; see fr-media's colour
// admission). Video range: Y 16..235, C 16..240.
static float3 bt709(float y, float2 c, bool fullRange) {
    float luma;
    float2 chroma;
    if (fullRange) {
        luma = y;
        chroma = c - 128.0 / 255.0;
    } else {
        luma = (y - 16.0 / 255.0) * (255.0 / 219.0);
        chroma = (c - 128.0 / 255.0) * (255.0 / 224.0);
    }
    float cb = chroma.x;
    float cr = chroma.y;
    return saturate(float3(luma + 1.5748 * cr,
                           luma - 0.1873 * cb - 0.4681 * cr,
                           luma + 1.8556 * cb));
}

// Crisp at exact integer scales (nearest luma), smooth when magnified by a
// fraction (bilinear), and box-filtered when the picture is shrunk so small
// text stays legible instead of aliasing: up to 4x4 taps per screen pixel.
fragment float4 picture_fragment(QuadOut in [[stage_in]],
                                 texture2d<float> lumaTexture [[texture(0)]],
                                 texture2d<float> chromaTexture [[texture(1)]],
                                 constant PictureUniforms &u [[buffer(0)]]) {
    constexpr sampler linearSampler(filter::linear, address::clamp_to_edge);
    constexpr sampler nearestSampler(filter::nearest, address::clamp_to_edge);
    bool fullRange = u.fullRange > 0.5;
    if (u.exact > 0.5) {
        float y = lumaTexture.sample(nearestSampler, in.uv).r;
        float2 c = chromaTexture.sample(linearSampler, in.uv).rg;
        return float4(bt709(y, c, fullRange), 1);
    }
    float2 step = fwidth(in.uv);
    float2 footprint = step * u.textureSize;
    int taps = clamp(int(ceil(max(footprint.x, footprint.y))), 1, 4);
    if (taps == 1) {
        float y = lumaTexture.sample(linearSampler, in.uv).r;
        float2 c = chromaTexture.sample(linearSampler, in.uv).rg;
        return float4(bt709(y, c, fullRange), 1);
    }
    float y = 0;
    float2 c = 0;
    for (int j = 0; j < taps; j++) {
        for (int i = 0; i < taps; i++) {
            float2 offset = (float2(i, j) + 0.5) / float(taps) - 0.5;
            float2 uv = in.uv + offset * step;
            y += lumaTexture.sample(linearSampler, uv).r;
            c += chromaTexture.sample(linearSampler, uv).rg;
        }
    }
    float n = float(taps * taps);
    return float4(bt709(y / n, c / n, fullRange), 1);
}

// The host's separately sent cursor (straight-alpha RGBA).
fragment float4 cursor_fragment(QuadOut in [[stage_in]], texture2d<float> shape [[texture(0)]]) {
    constexpr sampler nearestSampler(filter::nearest, address::clamp_to_edge);
    return shape.sample(nearestSampler, in.uv);
}
