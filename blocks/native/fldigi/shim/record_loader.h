/* Shim for <record_loader.h> — fldigi's data-file catalog loader. The
   real .cxx loads a station list off disk; headless we have none, so
   LoadAndRegister()->0 and the catalog stays empty (navtex still
   decodes text fine — it just won't enrich with station names). All
   non-pure so RecordLoader<Catalog> is concrete regardless of which
   methods the subclass overrides. */
#ifndef FERRITE_FLDIGI_SHIM_RECORD_LOADER_H
#define FERRITE_FLDIGI_SHIM_RECORD_LOADER_H
#include <string>
#include <utility>
#include <istream>
class RecordLoaderInterface {
public:
	RecordLoaderInterface() {}
	virtual ~RecordLoaderInterface() {}
	virtual void Clear() {}
	virtual bool ReadRecord(std::istream &) { return false; }
	int  LoadAndRegister() { return 0; }
	std::string ContentSize() const { return ""; }
	virtual std::string base_filename() const { return ""; }
	std::pair<std::string, bool> storage_filename(bool = false) const { return std::make_pair(std::string(), false); }
	virtual const char *Url() const { return 0; }
	virtual const char *Description() const { return ""; }
	std::string Timestamp() const { return ""; }
	static void SetDataDir(const std::string &) {}
};
template <class Catalog>
class RecordLoaderSingleton {
public:
	static Catalog &InstCatalog() { static Catalog s; return s; }
};
template <class Catalog>
struct RecordLoader : public RecordLoaderInterface, public RecordLoaderSingleton<Catalog> {};
inline void createRecordLoader() {}
#endif
