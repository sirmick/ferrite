/* Shim for <kmlserver.h> — fldigi's KML map-output server. navtex
   Broadcasts decoded station positions; headless has no KML sink, so
   everything is a no-op. Variadic templates dodge exact signatures. */
#ifndef FERRITE_FLDIGI_SHIM_KMLSERVER_H
#define FERRITE_FLDIGI_SHIM_KMLSERVER_H
#include <vector>
#include <string>
#include <utility>
class KmlServer {
public:
	struct CustomDataT : public std::vector<std::pair<std::string, std::string> > {
		template <class V> void Push(const char *, const V &) {}
		void Push(const char *, const char *) {}
	};
	template <class... A> void Broadcast(A &&...) {}
	int NbBroadcasts() const { return 0; }
	static KmlServer *GetInstance() { static KmlServer s; return &s; }
};
#endif
