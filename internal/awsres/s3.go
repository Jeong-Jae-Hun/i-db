package awsres

import (
	"context"
	"io"
	"mime"
	"path"
	"slices"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
)

// ─── S3 (R67.6) ───

type s3API interface {
	s3.ListBucketsAPIClient
	s3.ListObjectsV2APIClient
	GetBucketLocation(context.Context, *s3.GetBucketLocationInput, ...func(*s3.Options)) (*s3.GetBucketLocationOutput, error)
	HeadObject(context.Context, *s3.HeadObjectInput, ...func(*s3.Options)) (*s3.HeadObjectOutput, error)
	GetObject(context.Context, *s3.GetObjectInput, ...func(*s3.Options)) (*s3.GetObjectOutput, error)
}

const (
	// s3Level 은 한 단계(버킷 또는 접두)에서 보이는 접두·객체 수다. ListObjectsV2 한 페이지만 받는다
	s3Level = 1000
	// s3Peek 은 객체 Enter 가 본문을 보이는 상한이다. 이보다 큰 객체는 받지 않고, 받을 때도 Range 로 이만큼만 요청한다
	s3Peek = 64 << 10
)

// s3Children 은 rest "" 면 버킷, "<버킷>[/<접두>]" 면 그 접두 바로 아래 접두(/ 구분)와 객체다.
// 버킷 경로는 s3/<버킷>, 접두는 s3/<버킷>/<접두/>, 객체는 s3/<버킷>/<키> 다. 버킷 이름에는 / 가 없다.
func (b *Backend) s3Children(ctx context.Context, rest string) ([]source.Node, error) {
	if rest == "" {
		return b.buckets(ctx)
	}
	bucket, prefix, _ := strings.Cut(rest, "/")
	if prefix != "" && !strings.HasSuffix(prefix, "/") { // 객체는 잎이다
		return nil, nil
	}
	out, err := b.s3.ListObjectsV2(ctx, &s3.ListObjectsV2Input{Bucket: aws.String(bucket), Prefix: aws.String(prefix),
		Delimiter: aws.String("/"), MaxKeys: aws.Int32(s3Level)}, b.bucketRegion(ctx, bucket))
	if err != nil {
		return nil, wrap("s3:ListBucket", err)
	}
	var nodes []source.Node
	for _, p := range out.CommonPrefixes {
		full := aws.ToString(p.Prefix)
		nodes = append(nodes, source.Node{Name: strings.TrimPrefix(full, prefix), Path: "s3/" + bucket + "/" + full, HasChildren: true})
	}
	for _, o := range out.Contents {
		key := aws.ToString(o.Key)
		if key == prefix { // 콘솔이 만든 빈 "폴더" 객체
			continue
		}
		nodes = append(nodes, source.Node{Name: strings.TrimPrefix(key, prefix), Path: "s3/" + bucket + "/" + key,
			Info: db.HumanBytes(aws.ToInt64(o.Size)) + " · " + shortTime(aws.ToTime(o.LastModified)), Openable: true})
	}
	if aws.ToBool(out.IsTruncated) {
		nodes = append(nodes, source.Node{Name: "… 앞 " + strconv.Itoa(s3Level) + "개만 보임", Path: "#more:" + rest})
	}
	return nodes, nil
}

// buckets 는 버킷 목록이다(Info 는 리전). 리전은 버킷마다 기억해 두고 그 리전 엔드포인트로 묻는다:
// 리다이렉트를 따라가지 않아(R60.1) 다른 리전 버킷을 접속 리전으로 물으면 301 오류다.
func (b *Backend) buckets(ctx context.Context) ([]source.Node, error) {
	var out []source.Node
	p := s3.NewListBucketsPaginator(b.s3, &s3.ListBucketsInput{MaxBuckets: aws.Int32(1000)})
	for p.HasMorePages() {
		page, err := p.NextPage(ctx)
		if err != nil {
			return nil, wrap("s3:ListAllMyBuckets", err)
		}
		for _, bk := range page.Buckets {
			name, region := aws.ToString(bk.Name), aws.ToString(bk.BucketRegion)
			if region != "" {
				b.regions.Store(name, region)
			}
			out = append(out, source.Node{Name: name, Path: "s3/" + name, Info: region, HasChildren: true})
		}
	}
	return out, nil
}

// bucketRegion 은 버킷 리전으로 보내는 옵션이다. 목록에서 못 받았으면 GetBucketLocation 으로 묻고(빈 값은 us-east-1),
// 그것도 안 되면 접속 리전 그대로 보낸다.
func (b *Backend) bucketRegion(ctx context.Context, bucket string) func(*s3.Options) {
	r, ok := b.regions.Load(bucket)
	if !ok {
		if loc, err := b.s3.GetBucketLocation(ctx, &s3.GetBucketLocationInput{Bucket: aws.String(bucket)}); err == nil {
			r = string(loc.LocationConstraint)
			switch r {
			case "":
				r = "us-east-1"
			case "EU":
				r = "eu-west-1"
			}
			b.regions.Store(bucket, r)
		}
	}
	return func(o *s3.Options) {
		if s, _ := r.(string); s != "" {
			o.Region = s
		}
	}
}

// objectTable 은 객체 Enter 의 `attribute, value` 표다. HeadObject 로 메타데이터를 보이고, 텍스트로 보이는
// 64KiB 이하 객체만 본문을 붙인다. 본문은 Range bytes=0-65535 로 요청해 그 사이 객체가 커져도 64KiB 넘게 받지 않는다.
func (b *Backend) objectTable(ctx context.Context, rest string) (*db.Result, error) {
	bucket, key, _ := strings.Cut(rest, "/")
	region := b.bucketRegion(ctx, bucket)
	h, err := b.s3.HeadObject(ctx, &s3.HeadObjectInput{Bucket: aws.String(bucket), Key: aws.String(key)}, region)
	if err != nil {
		return nil, wrap("s3:GetObject", err) // HeadObject 의 IAM 작업은 s3:GetObject 다
	}
	size := aws.ToInt64(h.ContentLength)
	res := table("attribute", "value")
	res.Rows = append(res.Rows,
		[]string{"key", key},
		[]string{"size", strconv.FormatInt(size, 10)},
		[]string{"last_modified", longTime(aws.ToTime(h.LastModified))},
		[]string{"content_type", aws.ToString(h.ContentType)},
		[]string{"content_encoding", aws.ToString(h.ContentEncoding)},
		[]string{"etag", aws.ToString(h.ETag)},
		[]string{"storage_class", string(h.StorageClass)},
		[]string{"server_side_encryption", string(h.ServerSideEncryption)},
		[]string{"version_id", aws.ToString(h.VersionId)},
	)
	metaKeys := make([]string, 0, len(h.Metadata))
	for k := range h.Metadata {
		metaKeys = append(metaKeys, k)
	}
	slices.Sort(metaKeys)
	for _, k := range metaKeys {
		res.Rows = append(res.Rows, []string{"x-amz-meta-" + k, h.Metadata[k]})
	}
	switch {
	case !textLike(aws.ToString(h.ContentType), key) || (aws.ToString(h.ContentEncoding) != "" && aws.ToString(h.ContentEncoding) != "identity"):
	case size > s3Peek:
		res.Rows = append(res.Rows, []string{"body", "(64KiB 를 넘어 받지 않음)"})
	default:
		body, err := b.peek(ctx, bucket, key, region)
		if err != nil {
			return nil, err
		}
		if !utf8.ValidString(body) {
			body = "(텍스트가 아님)"
		}
		res.Rows = append(res.Rows, []string{"body", body})
	}
	return res, nil
}

func (b *Backend) peek(ctx context.Context, bucket, key string, region func(*s3.Options)) (string, error) {
	out, err := b.s3.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String(bucket), Key: aws.String(key),
		Range: aws.String("bytes=0-" + strconv.Itoa(s3Peek-1))}, region)
	if err != nil {
		return "", wrap("s3:GetObject", err)
	}
	defer out.Body.Close()
	data, err := io.ReadAll(io.LimitReader(out.Body, s3Peek))
	if err != nil {
		return "", wrap("s3:GetObject", err)
	}
	return string(data), nil
}

// textExts 는 Content-Type 이 비었거나 application/octet-stream 일 때 텍스트로 볼 확장자다.
var textExts = []string{".txt", ".log", ".csv", ".tsv", ".json", ".jsonl", ".ndjson", ".md", ".yaml", ".yml", ".xml", ".html", ".htm", ".sql", ".ini", ".toml"}

// textLike 는 본문을 글로 보일 만한 객체인지다. text/*, JSON·XML·YAML·JavaScript·CSV 꼴 Content-Type 이거나
// 형식을 모르는 객체의 확장자가 textExts 에 있으면 그렇다.
func textLike(contentType, key string) bool {
	mt, _, _ := mime.ParseMediaType(contentType)
	switch {
	case strings.HasPrefix(mt, "text/"):
		return true
	case mt == "application/json", mt == "application/xml", mt == "application/yaml", mt == "application/x-yaml",
		mt == "application/javascript", mt == "application/x-ndjson", mt == "application/csv",
		strings.HasSuffix(mt, "+json"), strings.HasSuffix(mt, "+xml"):
		return true
	case mt == "" || mt == "application/octet-stream" || mt == "binary/octet-stream":
		return slices.Contains(textExts, strings.ToLower(path.Ext(key)))
	}
	return false
}
